//! What a request is aimed at, once the session's lane has had its say.
//!
//! Before lanes, `window: null` meant `GetForegroundWindow()` — the same
//! answer for every agent on the broker, so two agents working at once drove
//! whatever the *user* happened to be looking at, and each could read the
//! other's screen. Spec §5 replaces that with two rules:
//!
//! * `window: null` is **this session's lane**, never the OS foreground.
//! * Windows of other lanes are invisible: `no_target`, not a silent hit.
//!
//! Both are enforced here, in one place, so `observe` and `act` cannot drift
//! apart about what a request pointed at.

use std::sync::Arc;

use kratos_protocol::{codes, Lane, LaneState, Source, TargetClass, WireError};
use serde_json::json;

use crate::classify::{self, WindowRef};
use crate::state::{Broker, PeerHandle};

/// Where a request is pointed.
pub struct Aim {
    /// The OS window, when one was found. `None` means the lane's browser is
    /// attached but has no visible window this broker could identify — the
    /// DOM backend can still reach its tabs, and the gate is judged against
    /// the synthesised reference in [`Aim::target_ref`].
    pub window: Option<WindowRef>,
    pub class: TargetClass,
    /// The lane's own extension instance, when one is attached and paired.
    /// The *specific peer*, not just the source: with two browsers attached,
    /// "the dom-ext backend" is not an answer.
    pub instance: Option<Arc<PeerHandle>>,
    pub lane: Option<Lane>,
    /// The `window` string to pass on to a backend. For a lane's own browser
    /// with no window named, this is `None`: the extension answers for its own
    /// active tab, which is the whole point of a lane.
    pub backend_window: Option<String>,
}

/// Which `dom-ext` peer, if any, may answer for this request.
#[derive(Clone, Copy)]
pub enum Dom<'a> {
    /// This lane's own instance, or nothing at all.
    ///
    /// A lane with no instance paired must **not** fall through to whichever
    /// `dom-ext` backend happens to be attached: that is another browser, and
    /// answering from it is precisely the cross-lane leak lanes exist to
    /// stop. Measured: a session in the Vivaldi lane read the Brave lane's
    /// Gmail tab, because "the dom-ext backend" was still a thing the router
    /// could find.
    Lane(Option<&'a Arc<PeerHandle>>),
    /// No lane at all (`browser: "none"`): any attached DOM backend will do,
    /// which is what this did before lanes existed.
    Any,
    /// Do not use the DOM backend: this dispatch is going to the OS.
    Skip,
}

impl Aim {
    /// Which DOM peer may serve this request.
    pub fn dom(&self) -> Dom<'_> {
        match self.lane {
            Some(_) => Dom::Lane(self.instance.as_ref()),
            None => Dom::Any,
        }
    }

    /// A window reference for the gate and the kill switch.
    ///
    /// Synthesised from the brand when no window was found, so a kill scope of
    /// `browser:brave` still covers a brave lane that is being driven through
    /// the extension with no window this broker could name.
    pub fn window_or_brand(&self) -> WindowRef {
        if let Some(w) = &self.window {
            return w.clone();
        }
        let brand = self.lane.as_ref().map(|l| l.browser.clone()).unwrap_or_default();
        WindowRef {
            hwnd: 0,
            title: self
                .lane
                .as_ref()
                .map(|l| format!("{} ({})", l.browser, l.profile_dir))
                .unwrap_or_default(),
            exe: if brand.is_empty() { String::new() } else { format!("{brand}.exe") },
            pid: 0,
            rect: None,
        }
    }
}

/// Which attached `dom-ext` instance belongs to this lane.
async fn instance_for(broker: &Arc<Broker>, lane: &Lane) -> Option<Arc<PeerHandle>> {
    let config = broker.config.read().await;
    broker.backends_for(Source::DomExt).await.into_iter().find(|p| {
        p.instance
            .as_ref()
            .and_then(|i| config.instance(&i.token))
            .is_some_and(|c| {
                c.brand.eq_ignore_ascii_case(&lane.browser) && c.profile_dir == lane.profile_dir
            })
    })
}

fn lane_unavailable(lane: &Lane, why: &str) -> WireError {
    WireError::with_detail(
        codes::LANE_UNAVAILABLE,
        format!(
            "this session's lane ({} / {}) could not be reached: {why}",
            lane.browser, lane.profile_dir
        ),
        json!({"browser": lane.browser, "profile_dir": lane.profile_dir,
               "state": lane.state.as_str(), "warning": lane.warning}),
    )
}

/// Resolve what a request is aimed at.
pub async fn aim(
    broker: &Arc<Broker>,
    peer: &Arc<PeerHandle>,
    window: Option<&str>,
) -> Result<Aim, WireError> {
    let lane = peer.lane.lock().await.clone();
    let named = window.map(str::trim).filter(|w| !w.is_empty());

    // A peer with no lane -- the settings window -- keeps the old behaviour:
    // it is the user, looking at their own screen.
    let Some(lane) = lane else {
        let win = legacy_window(named)?;
        let class = win.class();
        return Ok(Aim {
            class,
            window: Some(win),
            instance: None,
            lane: None,
            backend_window: named.map(|w| w.to_string()),
        });
    };

    if lane.state == LaneState::Unavailable {
        return Err(lane_unavailable(
            &lane,
            lane.warning.as_deref().unwrap_or("the browser is not available on this machine"),
        ));
    }

    let mut instance = instance_for(broker, &lane).await;
    let mut opened = None;

    // Nothing attached for this lane, and no window named: this is the case
    // the ensure-lane ladder exists for. Only for `window: null` -- a request
    // that names a window is asking about something that is already there,
    // and it is also how the ladder's own steps avoid recursing into it.
    if instance.is_none() && named.is_none() {
        match crate::ensure::ensure(broker, peer, &lane).await {
            Ok(outcome) => {
                if outcome == crate::ensure::Opened::Launched {
                    instance = instance_for(broker, &lane).await;
                }
                // What the ladder concluded, next to the launch it journalled
                // on the way: an operator reading the log should not have to
                // infer the outcome from what did not happen afterwards.
                broker.journal.read().await.append(json!({
                    "op": "lane.ensure", "peer_id": peer.peer_id, "agent": peer.name,
                    "harness": peer.harness_id,
                    "browser": lane.browser, "profile_dir": lane.profile_dir,
                    "outcome": outcome.as_str(),
                    "attached": instance.is_some(),
                }));
                opened = Some(outcome);
            }
            // A lane that cannot be opened is a lane the agent cannot work
            // in, and the error says which browser and which profile.
            Err(e) => return Err(e),
        }
    }

    match named {
        // -- the lane's own active tab --------------------------------------
        None => {
            // The brand's window, when there is one, so the gate and the kill
            // switch judge something real. Its absence is not fatal: a browser
            // can be running with its window minimised to the tray, and the
            // extension still answers for its tabs.
            let window = classify::windows_of_brand(&lane.browser).into_iter().next();
            if instance.is_none() && window.is_none() {
                return Err(lane_unavailable(
                    &lane,
                    "no window of that browser is open and no extension instance is paired with \
                     that profile",
                ));
            }
            // What the ladder did, carried through so the result can say
            // the lane is being driven without the extension rather than
            // leaving an agent to wonder why there are no selectors.
            let mut lane = lane;
            if let Some(outcome) = &opened {
                lane.state = if instance.is_some() {
                    LaneState::Ready
                } else {
                    outcome.state()
                };
                if let crate::ensure::Opened::Degraded(why) = outcome {
                    lane.warning = Some(why.clone());
                }
            }
            Ok(Aim {
                window,
                class: TargetClass::Browser,
                instance,
                lane: Some(lane),
                backend_window: None,
            })
        }

        // -- a named window -------------------------------------------------
        Some(name) => {
            let all = classify::find_all_by_title(name);
            let total = all.len();
            // Another lane's browser is not this session's to see. A native
            // window belongs to no lane and stays visible to everyone: there
            // is one Notepad, and the input lease is what serialises it.
            let mine: Vec<WindowRef> = all
                .into_iter()
                .filter(|w| match w.brand() {
                    Some(brand) => brand.eq_ignore_ascii_case(&lane.browser),
                    None => true,
                })
                .collect();

            if mine.is_empty() {
                let detail = if total > 0 {
                    format!(
                        "{name:?} matches {total} window(s), all of them another lane's browser. \
                         This session drives {} ({}); windows of other lanes are not visible to \
                         it.",
                        lane.browser, lane.profile_dir
                    )
                } else {
                    format!("no visible window matches {name:?}")
                };
                return Err(WireError::with_detail(
                    codes::NO_TARGET,
                    detail,
                    json!({"window": name, "matched_elsewhere": total,
                           "lane": {"browser": lane.browser, "profile_dir": lane.profile_dir}}),
                ));
            }

            let win = match classify::choose(mine) {
                classify::Resolution::One(w) => w,
                classify::Resolution::Ambiguous(all) => return Err(ambiguous(name, all)),
                classify::Resolution::NotFound => {
                    return Err(WireError::new(
                        codes::NO_TARGET,
                        format!("no visible window matches {name:?}"),
                    ))
                }
            };
            let is_mine_browser = win.brand().is_some();
            let class = win.class();
            Ok(Aim {
                window: Some(win),
                class,
                // A native window is not the extension's to act on, even for a
                // session whose lane is a browser.
                instance: if is_mine_browser { instance } else { None },
                lane: Some(lane),
                backend_window: Some(name.to_string()),
            })
        }
    }
}

/// The pre-lane rule, kept for peers that have no lane.
fn legacy_window(window: Option<&str>) -> Result<WindowRef, WireError> {
    match classify::resolve_window(window) {
        classify::Resolution::One(w) => Ok(w),
        classify::Resolution::Ambiguous(all) => Err(ambiguous(window.unwrap_or(""), all)),
        classify::Resolution::NotFound => Err(WireError::new(
            codes::NO_TARGET,
            match window {
                Some(w) => format!("no visible window matches {w:?}"),
                None => "there is no foreground window".to_string(),
            },
        )),
    }
}

pub fn ambiguous(window: &str, all: Vec<WindowRef>) -> WireError {
    let listed: Vec<String> =
        all.iter().take(6).map(|w| format!("{:?} ({})", w.title, w.exe)).collect();
    WireError::with_detail(
        codes::AMBIGUOUS_WINDOW,
        format!(
            "{window:?} matches windows of {} different applications, so it does not say which \
             one you mean: {}",
            all.iter()
                .map(|w| w.exe.to_ascii_lowercase())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            listed.join("; ")
        ),
        json!({ "candidates": all.iter().map(|w| json!({
            "title": w.title, "exe": w.exe, "class": w.class().as_str()
        })).collect::<Vec<_>>() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kratos_protocol::{LaneAssignment, LaneState};

    fn lane(browser: &str, profile: &str, state: LaneState) -> Lane {
        Lane {
            browser: browser.into(),
            profile_dir: profile.into(),
            profile_name: None,
            assigned: LaneAssignment::default(),
            state,
            warning: None,
        }
    }

    fn win(exe: &str, title: &str) -> WindowRef {
        WindowRef { hwnd: 7, title: title.into(), exe: exe.into(), pid: 1, rect: None }
    }

    /// The filter at the heart of cross-lane invisibility, in isolation.
    fn visible_to(lane: &Lane, all: Vec<WindowRef>) -> Vec<WindowRef> {
        all.into_iter()
            .filter(|w| match w.brand() {
                Some(brand) => brand.eq_ignore_ascii_case(&lane.browser),
                None => true,
            })
            .collect()
    }

    #[test]
    fn another_lanes_browser_is_not_visible() {
        let brave = lane("brave", "Default", LaneState::Ready);
        let all = vec![
            win("brave.exe", "Inbox - Gmail - Brave"),
            win("vivaldi.exe", "Inbox - Gmail - Vivaldi"),
        ];
        let seen = visible_to(&brave, all);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].exe, "brave.exe");
    }

    #[test]
    fn a_native_window_belongs_to_no_lane_and_everyone_sees_it() {
        // There is one Notepad and one keyboard; the input lease is what
        // serialises them, not visibility.
        let brave = lane("brave", "Default", LaneState::Ready);
        let vivaldi = lane("vivaldi", "Default", LaneState::Ready);
        let all = vec![win("notepad.exe", "Untitled - Notepad")];
        assert_eq!(visible_to(&brave, all.clone()).len(), 1);
        assert_eq!(visible_to(&vivaldi, all).len(), 1);
    }

    #[test]
    fn an_unavailable_lane_says_so_rather_than_falling_back_to_the_foreground() {
        let l = lane("opera", "Default", LaneState::Unavailable);
        let e = lane_unavailable(&l, "not installed");
        assert_eq!(e.code, codes::LANE_UNAVAILABLE);
        assert!(e.message.contains("opera"));
        assert_eq!(e.detail["state"], json!("unavailable"));
    }

    #[test]
    fn a_window_reference_is_synthesised_from_the_brand_when_none_was_found() {
        // Otherwise a kill scope of `browser:brave` would not cover a brave
        // lane being driven through the extension with no window in sight.
        let aim = Aim {
            window: None,
            class: TargetClass::Browser,
            instance: None,
            lane: Some(lane("brave", "Profile 2", LaneState::Ready)),
            backend_window: None,
        };
        let w = aim.window_or_brand();
        assert_eq!(w.exe, "brave.exe");
        assert_eq!(w.brand(), Some("brave"));
        assert!(w.title.contains("Profile 2"));
    }

    #[test]
    fn describing_the_machines_windows_is_cheap_enough_to_do_per_request() {
        // Lane filtering describes every visible window rather than matching
        // titles in the callback, because the question is about the process.
        let started = std::time::Instant::now();
        let all = classify::visible_windows();
        let each = started.elapsed();
        eprintln!("{} visible windows in {each:?}", all.len());
        assert!(
            each < std::time::Duration::from_millis(250),
            "too slow to do on every request: {each:?}"
        );
        for w in &all {
            assert!(!w.title.is_empty(), "a titleless window should have been skipped");
        }
    }
}
