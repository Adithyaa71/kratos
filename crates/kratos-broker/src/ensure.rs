//! Opening the browser a session needs.
//!
//! A lane is an allocation, not an observation: harness `claude` drives Brave
//! with profile `Work` whether or not Brave is running. So when an op needs
//! that lane and nothing is attached for it, something has to open it — and
//! the thing that opens it has to be as accountable as any other action,
//! because launching a browser with somebody's logged-in profile is not a
//! small thing to do quietly.
//!
//! The ladder is spec §4, in order, and it stops at the first rung that
//! works:
//!
//! 1. An instance is attached and paired. Done.
//! 2. `open_method: "direct"` — launch the browser with
//!    `--profile-directory`, which opens a window for that profile whether or
//!    not the browser is already running, and wait for an extension instance
//!    to attach. Exactly one unbound instance appearing is that profile's.
//! 3. `open_method: "ui"` — drive the browser's own interface with the
//!    decision model, bounded.
//! 4. A direct launch blocked by something unexpected — a profile picker, a
//!    restore-pages bubble, a sign-in prompt — falls into the same bounded
//!    recovery.
//! 5. Nothing ever attaches: the lane is `degraded`, driven through UI
//!    Automation, and the result says so.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kratos_protocol::{codes, Lane, LaneState, WireError};
use serde_json::json;

use crate::browsers;
use crate::state::{Broker, PeerHandle};

/// How long a lane that could not be opened is left alone.
///
/// The ladder runs when a lane is not ready, and a lane that stays degraded
/// is not ready on the next request either -- so without this, every single
/// observation pays for a launch, a wait and three recovery steps. Measured
/// on a browser with no reachable extension: fifteen seconds per observe,
/// for ever.
///
/// Thirty seconds is long enough that a session working in a degraded lane
/// pays once, and short enough that an operator who fixes the extension and
/// comes back does not have to restart anything.
const COOLDOWN: Duration = Duration::from_secs(30);

/// How long to wait for an extension instance after a launch.
///
/// The spec's figure. A cold Chromium start on a loaded machine is a second
/// or two to a window and a little more to a service worker; eight is
/// generous without being a hang.
const ATTACH_WAIT: Duration = Duration::from_secs(8);

/// How often to look while waiting.
const POLL: Duration = Duration::from_millis(250);

/// The most Jev-driven steps one lane may cost.
///
/// Spec §4.3. A bound rather than a timeout because each step is a full act —
/// observe, decide, gate, execute, prove — and ten of those is already
/// several seconds of somebody's machine being driven on the broker's
/// initiative rather than the agent's.
const MAX_STEPS: usize = 10;

/// What happened, for the lane's state and for the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// Already there.
    Attached,
    /// Launched, and an instance attached and was paired.
    Launched,
    /// Launched, nothing attached: drivable through UI Automation only.
    Degraded(String),
    /// The operator has turned opening off.
    NotAttempted,
}

impl Opened {
    pub fn state(&self) -> LaneState {
        match self {
            Opened::Attached | Opened::Launched => LaneState::Ready,
            Opened::Degraded(_) | Opened::NotAttempted => LaneState::Degraded,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Opened::Attached => "attached",
            Opened::Launched => "launched",
            Opened::Degraded(_) => "degraded",
            Opened::NotAttempted => "not_attempted",
        }
    }
}

/// Make this lane reachable, or say why it cannot be.
pub async fn ensure(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    lane: &Lane,
) -> Result<Opened, WireError> {
    // Rung 1.
    if lane.state == LaneState::Ready {
        return Ok(Opened::Attached);
    }

    // Rung 1b: an instance of this browser is already attached and nobody has
    // said which profile it is.
    //
    // The spec's rule -- "if exactly one unbound instance appears, bind it"
    // -- read as being about the instance that appears *after* a launch, and
    // on a machine where the browser was already open with the extension
    // running there is nothing to appear. Launching to produce an event that
    // has already happened is a window opening for no reason, so the case is
    // taken first. Exactly one, for the same reason as after a launch:
    // "whichever" is a coin toss, and binding the wrong token points a lane
    // at another profile's tabs.
    let unbound_here = unbound_of_brand(broker, &lane.browser).await;
    if let [token] = unbound_here.as_slice() {
        bind(broker, token, &lane.browser, &lane.profile_dir).await?;
        broker.journal.read().await.append(json!({
            "op": "instance.bind", "peer_id": peer.peer_id, "agent": peer.name,
            "token": token, "brand": lane.browser, "profile_dir": lane.profile_dir,
            "by": "lane.adopt",
        }));
        return Ok(Opened::Launched);
    }

    // Tried recently and it did not work: do not pay for it again.
    if let Some(why) = broker.lane_cooling(&lane.browser, &lane.profile_dir, COOLDOWN).await {
        return Ok(Opened::Degraded(why));
    }

    let (method, open_timeout) = {
        let cfg = broker.config.read().await;
        (cfg.defaults.open_method.trim().to_ascii_lowercase(), cfg.defaults.pause_max_ms)
    };
    let _ = open_timeout;

    if method == "none" || method == "off" {
        // A deliberate allocation, not an omission: a machine where kratos
        // may drive a browser that is open but may not open one.
        return Ok(Opened::NotAttempted);
    }

    let located = browsers::locate(&lane.browser).filter(|b| b.installed());
    let Some(browser) = located else {
        return Err(WireError::with_detail(
            codes::LANE_UNAVAILABLE,
            format!(
                "{} is not installed on this machine, so this session's lane cannot be opened",
                lane.browser
            ),
            json!({"browser": lane.browser, "profile_dir": lane.profile_dir}),
        ));
    };

    // Opening a browser is an action on the machine, so it answers to the
    // same things every action answers to. The kill switch first, because a
    // halted system must not start a process.
    {
        let gate = broker.gate.lock().await;
        let target = kratos_gate::TargetRef {
            window_title: format!("{} ({})", lane.browser, lane.profile_dir),
            url: None,
            exe: Some(format!("{}.exe", lane.browser)),
            brand: Some(lane.browser.clone()),
        };
        if let Some(scope) = gate.kill.covers(&target) {
            return Err(WireError::with_detail(
                codes::KILL_SWITCH,
                format!("the kill switch is engaged for {scope}, so no browser will be opened"),
                json!({"scope": scope}),
            ));
        }
    }

    let before: Vec<String> = unbound_tokens(broker).await;

    // Rung 2: the direct launch. `--profile-directory` opens a window for
    // that profile inside a browser that is already running, which is why
    // this works whether or not one is -- and why it is tried before anything
    // that drives a picker.
    let exe = browser.exe.clone().unwrap_or_default();
    let profile = lane.profile_dir.clone();
    let spawned = tokio::process::Command::new(&exe)
        .arg(format!("--profile-directory={profile}"))
        .spawn();

    let launch_error = match spawned {
        Ok(mut child) => {
            // Detached on purpose: the browser outlives the broker, and a
            // broker that reaped it on exit would close the operator's
            // windows.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            None
        }
        Err(e) => Some(e.to_string()),
    };

    broker.journal.read().await.append(json!({
        "op": "lane.open", "peer_id": peer.peer_id, "agent": peer.name,
        "harness": peer.harness_id, "browser": lane.browser, "profile_dir": lane.profile_dir,
        "method": "direct", "exe": exe.display().to_string(),
        "error": launch_error,
    }));

    if let Some(e) = launch_error {
        return Err(WireError::with_detail(
            codes::LANE_UNAVAILABLE,
            format!("could not start {}: {e}", exe.display()),
            json!({"browser": lane.browser, "profile_dir": lane.profile_dir}),
        ));
    }

    // Rung 2, second half: wait for the extension, and pair whatever turns up.
    if let Some(token) = wait_for_new_instance(broker, &before).await {
        bind(broker, &token, &lane.browser, &lane.profile_dir).await?;
        broker.journal.read().await.append(json!({
            "op": "instance.bind", "peer_id": peer.peer_id, "agent": peer.name,
            "token": token, "brand": lane.browser, "profile_dir": lane.profile_dir,
            "by": "lane.open",
        }));
        return Ok(Opened::Launched);
    }

    // Rungs 3 and 4: the window is up and something is in front of what we
    // wanted -- a profile picker, a restore-pages bubble, a sign-in nudge.
    // Bounded, gated, journalled, and each step a normal act.
    if !crate::classify::windows_of_brand(&lane.browser).is_empty() {
        if let Some(token) = recover(broker, peer, lane, &before).await {
            bind(broker, &token, &lane.browser, &lane.profile_dir).await?;
            broker.journal.read().await.append(json!({
                "op": "instance.bind", "peer_id": peer.peer_id, "agent": peer.name,
                "token": token, "brand": lane.browser, "profile_dir": lane.profile_dir,
                "by": "lane.recover",
            }));
            return Ok(Opened::Launched);
        }
    }

    // Rung 5. A window may well be there -- the launch worked -- and UI
    // Automation can drive it; what is missing is the extension, and saying
    // so is the whole point of `degraded`.
    let window_now = !crate::classify::windows_of_brand(&lane.browser).is_empty();
    let why = if window_now {
        format!(
            "{} is open with profile {:?} but no extension instance attached within {}s, so \
             this lane is driven through UI Automation: no container, no selector and no URL",
            lane.browser,
            lane.profile_dir,
            ATTACH_WAIT.as_secs()
        )
    } else {
        format!(
            "{} was launched and no window of it appeared within {}s",
            lane.browser,
            ATTACH_WAIT.as_secs()
        )
    };
    broker.lane_cooled(&lane.browser, &lane.profile_dir, &why).await;
    Ok(Opened::Degraded(why))
}

/// Attached, unpaired instances claiming this brand.
async fn unbound_of_brand(broker: &Arc<Broker>, brand: &str) -> Vec<String> {
    crate::lane::attached_instances(broker)
        .await
        .into_iter()
        .filter(|i| i.bound_to.is_none())
        .filter(|i| i.brand.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(brand)))
        .map(|i| i.token)
        .collect()
}

/// Drive whatever is in the way, one step at a time.
///
/// Each step is a real `act`: observed, decided by the model, gated, leased,
/// executed and proved, and written to the journal like any other. That is
/// the point of doing it this way rather than hunting for a button by name --
/// a broker that clicked things on its own initiative outside the gate would
/// be a second, unaudited way to drive the machine.
///
/// Stops the moment an instance attaches. A step that fails is not fatal:
/// the next rung may be the one that matches what is actually on screen.
async fn recover(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    lane: &Lane,
    before: &[String],
) -> Option<String> {
    let window = crate::classify::windows_of_brand(&lane.browser).into_iter().next()?;
    let profile_name = lane.profile_name.clone().unwrap_or_default();

    for (i, instruction) in recovery_steps(&profile_name).into_iter().enumerate() {
        let req = kratos_protocol::ActReq {
            instruction: instruction.clone(),
            // By title, which is also what keeps this from recursing: a
            // request that names a window never asks for the lane to be
            // opened.
            window: Some(window.title.clone()),
            dry_run: false,
            lease: None,
            target_id: None,
            timeout_ms: 8_000,
        };
        // Boxed because this re-enters the pipeline that called the ladder,
        // and a future cannot contain itself. It cannot recurse at run time
        // -- this request names a window, and only a null window asks for a
        // lane to be opened -- but the compiler is right to want the
        // indirection before taking that on trust.
        let outcome = Box::pin(crate::act::act(broker, peer, req)).await;
        broker.journal.read().await.append(json!({
            "op": "lane.recover", "peer_id": peer.peer_id, "agent": peer.name,
            "step": i + 1, "instruction": instruction,
            "status": outcome.as_ref().map(|r| r.status.clone()).ok(),
            "error": outcome.as_ref().err().map(|e| e.code.clone()),
        }));

        if let Some(token) = settle_for_instance(broker, before).await {
            return Some(token);
        }
    }
    None
}

/// A short wait between recovery steps.
///
/// The full eight seconds per step would make a three-step recovery feel
/// like a hang, and a service worker that is going to start after a click
/// starts within a second or two of it.
async fn settle_for_instance(broker: &Arc<Broker>, before: &[String]) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(2_000);
    while Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
        let fresh: Vec<String> = unbound_tokens(broker)
            .await
            .into_iter()
            .filter(|t| !before.contains(t))
            .collect();
        if fresh.len() == 1 {
            return fresh.into_iter().next();
        }
    }
    None
}

/// Tokens of attached `dom-ext` instances that no config entry pairs.
async fn unbound_tokens(broker: &Arc<Broker>) -> Vec<String> {
    crate::lane::attached_instances(broker)
        .await
        .into_iter()
        .filter(|i| i.bound_to.is_none())
        .map(|i| i.token)
        .collect()
}

/// Wait for exactly one new unbound instance.
///
/// Exactly one, because "whichever appeared" is a coin toss when two browsers
/// start together, and binding the wrong token points a lane at another
/// profile's tabs -- which is the failure this whole design is about.
async fn wait_for_new_instance(broker: &Arc<Broker>, before: &[String]) -> Option<String> {
    let deadline = Instant::now() + ATTACH_WAIT;
    while Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
        let now = unbound_tokens(broker).await;
        let fresh: Vec<String> =
            now.into_iter().filter(|t| !before.contains(t)).collect();
        match fresh.len() {
            0 => continue,
            1 => return fresh.into_iter().next(),
            _ => {
                eprintln!(
                    "kratos-broker: {} unbound instances appeared at once; pairing needs a \
                     human in the settings window",
                    fresh.len()
                );
                return None;
            }
        }
    }
    None
}

/// Pair a token with a profile, the same write `instance.bind` makes.
async fn bind(
    broker: &Arc<Broker>,
    token: &str,
    brand: &str,
    profile_dir: &str,
) -> Result<(), WireError> {
    let patch = {
        let cfg = broker.config.read().await;
        let mut list = cfg.instances.clone();
        list.retain(|i| i.token != token);
        list.push(kratos_protocol::config::InstanceConfig {
            token: token.to_string(),
            brand: brand.to_string(),
            profile_dir: profile_dir.to_string(),
        });
        json!({"instances": list})
    };
    broker.apply_config(&patch, None).await.map(|_| ())
}

/// The instructions the recovery is allowed to try, in order.
///
/// **The broker writes these, and that is worth being precise about.** The
/// invariant is that the decision model generates no text; it is not that no
/// text exists. The literal here is a profile *name read out of the browser's
/// own `Local State`* — machine state the user themselves allocated — and the
/// model's job is still only to decide which element on screen it means.
/// Nothing here comes from a page, a model or an agent.
pub fn recovery_steps(profile_name: &str) -> Vec<String> {
    let mut steps = Vec::new();
    if !profile_name.trim().is_empty() {
        steps.push(format!("click the {profile_name:?} profile"));
    }
    steps.extend([
        // A restore-pages bubble, a default-browser prompt, a sign-in nudge:
        // all of them sit over the window and none of them is the lane.
        "click the \"Close\" button".to_string(),
        "press Escape".to_string(),
    ]);
    steps.truncate(MAX_STEPS);
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ladder_outcome_maps_to_a_lane_state() {
        assert_eq!(Opened::Attached.state(), LaneState::Ready);
        assert_eq!(Opened::Launched.state(), LaneState::Ready);
        // Degraded is drivable, which is why it is not an error.
        assert_eq!(Opened::Degraded("x".into()).state(), LaneState::Degraded);
        assert_eq!(Opened::NotAttempted.state(), LaneState::Degraded);
    }

    #[test]
    fn recovery_is_bounded_and_names_the_profile_the_user_allocated() {
        let steps = recovery_steps("Work");
        assert!(steps.len() <= MAX_STEPS);
        assert!(steps[0].contains("\"Work\""), "{:?}", steps[0]);
        // Every literal in a step is quoted, because the broker's own
        // instructions go through the same extraction as an agent's.
        assert!(steps.iter().any(|s| s.contains("Escape")));

        // A browser with one nameless profile still gets the generic rungs.
        let plain = recovery_steps("   ");
        assert!(!plain.is_empty());
        assert!(!plain[0].contains("profile"));
    }
}
