//! P0 of docs/design-multi-harness.md: reproduce the four defects, fix nothing.
//!
//! Every test here asserts the behaviour the spec requires and **fails against
//! this build**. They are `#[ignore]`d so `cargo test --workspace` stays green
//! while the reproduction is in the tree and runnable:
//!
//! ```text
//! cargo test -p kratos-broker --test multi_harness_audit -- --ignored
//! ```
//!
//! Each one collects every gap it finds and asserts the list is empty at the
//! end, rather than stopping at the first assertion. A test that aborts on gap
//! one hides gaps two and three, and this file exists to enumerate them.
//!
//! The phase that fixes a defect removes that test's `#[ignore]`; a test still
//! ignored is a defect still present.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kratos_protocol::{codes, ops, Role};
use serde_json::{json, Value};

const BUILD: &str = "p0-audit";

/// Long enough for a backend to finish its handshake before the next step
/// looks for it. Deliberately the same figure the existing integration tests
/// use, so a timing difference is never the explanation for a difference in
/// outcome.
const SETTLE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Harness: one broker process, its own pipe, config and journal
// ---------------------------------------------------------------------------

struct Harness {
    pipe: String,
    dir: PathBuf,
    child: Option<Child>,
}

impl Harness {
    fn start(label: &str) -> Harness {
        // Its own input guard: isolated from every other broker on the
        // machine, including the operator's.
        let guard = format!("kratos-p0-{}-{}", std::process::id(), label);
        Harness::start_with_guard(label, &guard)
    }

    /// Two brokers that deliberately share one machine-wide input guard, which
    /// is what a second broker on a real machine would be.
    fn start_sharing_input(label: &str) -> Harness {
        let shared = format!("kratos-p0-shared-{}", std::process::id());
        Harness::start_with_guard(label, &shared)
    }

    fn start_with_guard(label: &str, guard: &str) -> Harness {
        let unique = format!("kratos-p0-{}-{}", std::process::id(), label);
        let dir = std::env::temp_dir().join(&unique);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = json!({
            "idle_exit_ms": 0,
            "decider": {"provider": "mock", "model": "test"},
            // Approved up front: an unapproved harness may call status and
            // whoami and nothing else, and the approval is a dialog no test
            // can click. Harness approval has its own test below, which uses
            // a name that is deliberately absent from this list.
            "harnesses": [
                {"id": "auditor", "approved": true},
                {"id": "claude", "approved": true},
                {"id": "bluee", "approved": true},
                {"id": "harness-one", "approved": true},
                {"id": "harness-two", "approved": true},
            ],
            "kill": {"hotkey": "ctrl+alt+shift+f23", "hotkey_scope": "global"},
            "gate": {
                "block_keywords": [], "review_keywords": [], "review_apps": [],
                "review_domains": [], "rate_limit_per_minute": 100,
                "require_os_dialog_for": [], "confirm_ttl_ms": 60000,
                "review_dialog_timeout_s": 5
            },
            "settle_max_ms": 120,
            // The ladder is not run by these tests: a suite that opened a
            // browser on this machine every time it checked lane resolution
            // would be a suite nobody runs twice. P5 has its own tests for
            // opening, which set this deliberately.
            "defaults": {"profile": "Default", "open_method": "none"},
            "thresholds": {"default": 0.0, "click": 0.0, "type": 0.0, "key": 0.0,
                           "scroll": 0.0, "select": 0.0, "navigate": 0.0,
                           "open": 0.0, "focus": 0.0},
            // The fake backends below are the point of this file; a spawned
            // kratos-native registers as `uia` too and would displace one.
            "backends": {"enabled": ["dom-ext", "uia", "pixel", "ocr"],
                         "spawn_native": false}
        });
        let config_path = dir.join("config.json");
        let mut f = std::fs::File::create(&config_path).expect("config");
        f.write_all(serde_json::to_string_pretty(&config).unwrap().as_bytes()).unwrap();
        drop(f);

        let child = Command::new(env!("CARGO_BIN_EXE_kratos-broker"))
            .env("KRATOS_PIPE", &unique)
            // Its own machine-wide input guard, so brokers started by
            // other tests -- and the one the operator may have running --
            // are not competing for this one.
            .env("KRATOS_INPUT_GUARD", guard)
            .env("KRATOS_CONFIG", &config_path)
            .env("KRATOS_JOURNAL", dir.join("actions.jsonl"))
            .env_remove("KRATOS_API_KEY")
            .env_remove("TYPESAFE_API_KEY")
            .env_remove("JEV_API_KEY")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kratos-broker");

        Harness { pipe: unique, dir, child: Some(child) }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn wait_for_pipe(pipe: &str) {
    for _ in 0..200 {
        if kratos_ipc::connect(pipe).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("broker never came up on {pipe}");
}

async fn agent(pipe: &str, name: &str) -> kratos_ipc::PipePeer {
    kratos_ipc::PipePeer::attach(pipe, Role::Agent, name, BUILD).await.expect("agent attach")
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// Attach as one session of one harness.
///
/// The hello is the shared one: `Hello::agent(..).with_identity(..)` is exactly
/// what `kratos-mcp` sends, so these tests exercise the handshake the real
/// shim uses rather than a hand-rolled copy of it.
async fn session(
    pipe: &str,
    harness: &str,
    agent: Option<&str>,
    session: &str,
) -> kratos_ipc::PipePeer {
    let hello = kratos_protocol::Hello::agent(harness, BUILD)
        .with_identity(Some(harness), agent, Some(session));
    kratos_ipc::PipePeer::attach_as(pipe, hello).await.expect("session attach")
}

async fn settings(pipe: &str) -> kratos_ipc::PipePeer {
    kratos_ipc::PipePeer::attach(pipe, Role::Ui, "settings", BUILD).await.expect("ui attach")
}

// ---------------------------------------------------------------------------
// A fake browser instance
// ---------------------------------------------------------------------------

/// One `dom-ext` peer, standing in for one browser profile instance.
///
/// It reports a window and element ids of its own brand, so an observation can
/// be traced back to the instance that answered it -- which is the whole
/// question in test (b).
#[derive(Clone)]
struct FakeInstance {
    source: String,
    brand: String,
    /// The instance token the spec has the extension generate per profile.
    token: String,
    window: String,
    url: String,
}

struct Live {
    /// Set when the broker sends `evt superseded` to this instance.
    superseded: Arc<AtomicBool>,
    /// Set when the broker drops the connection.
    closed: Arc<AtomicBool>,
    /// How many actions this instance was asked to carry out. Two lanes each
    /// acting once is how "they did not cross" is checked.
    acted: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeInstance {
    fn dom_ext(brand: &str, token: &str) -> Self {
        FakeInstance::dom_ext_brand(brand, token)
    }

    fn dom_ext_brand(brand: &str, token: &str) -> Self {
        FakeInstance {
            source: "dom-ext".to_string(),
            brand: brand.to_string(),
            token: token.to_string(),
            window: format!("Inbox - Gmail - {brand}"),
            url: "https://mail.google.com/mail/u/0/#inbox".to_string(),
        }
    }

    /// A backend with **no** instance, which is what `uia`, `ocr` and `pixel`
    /// are: one per machine.
    ///
    /// The empty token is the point. `Hello::with_instance` drops a token that
    /// is only whitespace, so this also proves an empty one is treated as no
    /// instance rather than as a shared registry slot that two peers could
    /// both claim.
    fn uia() -> Self {
        FakeInstance {
            source: "uia".to_string(),
            brand: "none".to_string(),
            token: "   ".to_string(),
            window: "Untitled - Notepad".to_string(),
            url: String::new(),
        }
    }

    fn elements(&self) -> Vec<Value> {
        vec![
            // `brand:wN:tN:fN:eN`, the shape the extension really mints --
            // the frame segment is what tells a tab id apart from a UIA
            // runtime id, and tab ownership reads it.
            json!({"id": format!("{}:w1:t1:f0:e0", self.brand), "role": "button", "label": "Send",
                   "container": "dialog:New Message", "bbox": [100, 200, 80, 24],
                   "selector": "div[role=dialog] button", "enabled": true}),
            json!({"id": format!("{}:w1:t1:f0:e1", self.brand), "role": "link", "label": "Inbox",
                   "container": "nav:Mail", "bbox": [10, 60, 70, 20],
                   "selector": "nav a", "enabled": true}),
        ]
    }

    /// Attach through the shared handshake, carrying the instance object from
    /// section 7 of the spec, and serve until the broker lets go.
    fn spawn(self, pipe: String) -> Live {
        let superseded = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let acted = Arc::new(AtomicUsize::new(0));
        let sup = superseded.clone();
        let cl = closed.clone();
        let did = acted.clone();
        let task = tokio::spawn(async move {
            let instance = kratos_protocol::Instance {
                token: self.token.clone(),
                brand: Some(self.brand.clone()),
                pid: Some(4242),
                parent_hwnd: None,
            };
            let mut peer = kratos_ipc::PipePeer::attach_backend_instance(
                &pipe,
                &format!("fake-{}-{}", self.source, self.brand),
                BUILD,
                &self.source,
                vec!["browser".to_string(), "native".to_string()],
                Some(instance),
            )
            .await
            .expect("backend attach");

            loop {
                let frame = match peer.next_frame().await {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        cl.store(true, Ordering::SeqCst);
                        return;
                    }
                    Err(e) if e.is_fatal() => {
                        cl.store(true, Ordering::SeqCst);
                        return;
                    }
                    Err(_) => continue,
                };
                if frame.op() == Some(ops::EVT_SUPERSEDED) {
                    sup.store(true, Ordering::SeqCst);
                    continue;
                }
                let (Some(op), Some(id)) = (frame.op(), frame.id()) else { continue };
                let id = id.to_string();
                match op {
                    ops::BACKEND_OBSERVE => {
                        let _ = peer
                            .reply(
                                &id,
                                json!({
                                    "window": self.window,
                                    "url": self.url,
                                    "elements": self.elements(),
                                    "truncated": false
                                }),
                            )
                            .await;
                    }
                    ops::BACKEND_ACT => {
                        did.fetch_add(1, Ordering::SeqCst);
                        let _ =
                            peer.reply(&id, json!({"applied": true, "took_ms": 3})).await;
                    }
                    _ => {}
                }
            }
        });
        Live { superseded, closed, acted, task }
    }
}

/// Whether an act reached a backend at all.
///
/// `executed` and `no_effect` both mean dispatched; against a fake backend
/// nothing on screen moves, so `no_effect` is the honest verdict and a test
/// about routing or ownership must not demand evidence the fake cannot
/// produce.
fn ran(res: &Value) -> bool {
    matches!(res["status"].as_str(), Some("executed") | Some("no_effect"))
}

fn backend_sources(status: &Value, source: &str) -> usize {
    status["backends"]
        .as_array()
        .map(|a| a.iter().filter(|b| b["source"] == json!(source)).count())
        .unwrap_or(0)
}

/// Name the real cause when an observation was routed by whatever had focus.
///
/// `observe` classifies the **OS foreground** window and routes on that, so in
/// a test run -- where the foreground is a terminal -- a browser instance is
/// never consulted at all and the reply is `no_backend` for a *native* window.
/// Saying so turns a confusing error into the finding itself.
fn foreground_note(e: &kratos_protocol::WireError) -> String {
    match e.detail.get("class").and_then(|c| c.as_str()) {
        Some(class) => format!(
            " -- window:null resolved to the OS foreground, classified {class:?}, so the \
             session's own lane was never consulted"
        ),
        None => String::new(),
    }
}

/// Fail with every gap listed, not just the first.
fn report(what: &str, gaps: Vec<String>) {
    if gaps.is_empty() {
        return;
    }
    let body =
        gaps.iter().enumerate().map(|(i, g)| format!("  {}. {g}", i + 1)).collect::<Vec<_>>();
    panic!("{what}: {} gap(s)\n{}", gaps.len(), body.join("\n"));
}

// ---------------------------------------------------------------------------
// (a) Two dom-ext instances: the older gets kicked
// ---------------------------------------------------------------------------

/// Spec 7: the backend registry key is `(source, instance.token)` and only the
/// *same* key supersedes. Two browser profiles are two instances of one source;
/// before P1 the second evicted the first, so one harness opening its browser
/// silently unplugged another's.
#[tokio::test]
async fn two_browser_instances_both_stay_registered() {
    let h = Harness::start("twoinstances");
    wait_for_pipe(&h.pipe).await;

    let brave = FakeInstance::dom_ext("brave", "tok-brave").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    let vivaldi = FakeInstance::dom_ext("vivaldi", "tok-vivaldi").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    let mut a = agent(&h.pipe, "auditor").await;
    let status = a.call(ops::STATUS, json!({})).await.expect("status");

    let mut gaps = Vec::new();
    let count = backend_sources(&status, "dom-ext");
    if count != 2 {
        gaps.push(format!(
            "status lists {count} dom-ext backend(s); two attached instances must both be \
             registered under (source, instance.token)"
        ));
    }
    if brave.superseded.load(Ordering::SeqCst) {
        gaps.push(
            "the first instance was sent `evt superseded` by a different instance; only the \
             same (source, token) may supersede"
                .to_string(),
        );
    }
    if brave.closed.load(Ordering::SeqCst) {
        gaps.push("the first instance's connection was dropped".to_string());
    }
    // Each entry names which instance it is, or the settings window has no way
    // to show the operator two browsers rather than one mystery backend.
    let tokens: Vec<String> = status["backends"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|b| b["source"] == json!("dom-ext"))
                .map(|b| b["instance"]["token"].as_str().unwrap_or("<none>").to_string())
                .collect()
        })
        .unwrap_or_default();
    if tokens != vec!["tok-brave".to_string(), "tok-vivaldi".to_string()] {
        gaps.push(format!(
            "status should list both instance tokens in a stable order, got {tokens:?}"
        ));
    }

    brave.task.abort();
    vivaldi.task.abort();
    report("two dom-ext instances", gaps);
}

/// The other half of P1's acceptance: the self-healing the protocol has always
/// promised must survive being narrowed. One instance reconnecting -- a service
/// worker restart, a browser relaunch into the same profile -- still replaces
/// itself, so a dead entry is never left for routing to keep choosing.
#[tokio::test]
async fn the_same_instance_reconnecting_supersedes_itself() {
    let h = Harness::start("samereconnect");
    wait_for_pipe(&h.pipe).await;

    let first = FakeInstance::dom_ext("brave", "tok-brave").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    let second = FakeInstance::dom_ext("brave", "tok-brave").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    let mut a = agent(&h.pipe, "auditor").await;
    let status = a.call(ops::STATUS, json!({})).await.expect("status");

    let mut gaps = Vec::new();
    let count = backend_sources(&status, "dom-ext");
    if count != 1 {
        gaps.push(format!("one instance reconnecting left {count} entries, not 1"));
    }
    if !first.superseded.load(Ordering::SeqCst) {
        gaps.push("the displaced connection was not told it had been superseded".to_string());
    }

    // And a source with no instance at all still behaves as it always did.
    let _uia = FakeInstance::uia().spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    let _uia2 = FakeInstance::uia().spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    let status = a.call(ops::STATUS, json!({})).await.expect("status");
    if backend_sources(&status, "uia") != 1 {
        gaps.push(format!(
            "a backend that sends no instance must stay one per source, got {}",
            backend_sources(&status, "uia")
        ));
    }

    first.task.abort();
    second.task.abort();
    report("same instance reconnecting", gaps);
}

// ---------------------------------------------------------------------------
// (b) observe window:null reads another agent's window
// ---------------------------------------------------------------------------

/// Spec 3 and 5: a session resolves to a lane, `window:null` means *that
/// lane's* active tab and never the OS foreground, and another lane's windows
/// are invisible (`no_target`). Today `window:null` is
/// `GetForegroundWindow()`, the same answer for every agent on the broker, and
/// nothing in the handshake even says which lane a session is in.
#[tokio::test]
async fn a_session_observes_its_own_lane_not_another_agents_window() {
    let h = Harness::start("lanescope");
    wait_for_pipe(&h.pipe).await;

    // One attached instance, standing in for the Brave lane of harness
    // "claude". Harness "bluee" is allocated Vivaldi and has no instance.
    let brave = FakeInstance::dom_ext("brave", "tok-brave").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    let mut ui = settings(&h.pipe).await;
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [
                {"id": "claude", "browser": "brave", "approved": true},
                {"id": "bluee", "browser": "vivaldi", "approved": true},
            ],
            "instances": [{"token": "tok-brave", "brand": "brave", "profile_dir": "Default"}],
            "defaults": {"profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    let mut one = session(&h.pipe, "claude", Some("agent-a"), "s-1").await;
    let mut two = session(&h.pipe, "bluee", Some("agent-b"), "s-2").await;

    let mut gaps = Vec::new();

    for (who, peer) in [("claude/agent-a", &one), ("bluee/agent-b", &two)] {
        if peer.ack.get("lane").is_none() {
            gaps.push(format!("the hello reply to {who} carries no `lane` object"));
        }
        if peer.ack.get("approved").is_none() {
            gaps.push(format!("the hello reply to {who} carries no `approved` flag"));
        }
    }

    // whoami is how an agent reads its own lane back.
    match two.call("whoami", json!({})).await {
        Ok(v) => {
            if v.get("lane").is_none() {
                gaps.push("whoami answered without a `lane` object".to_string());
            }
        }
        Err(e) => gaps.push(format!("whoami is not implemented: {} {}", e.code, e.message)),
    }

    // The Brave lane's own session may read it.
    match one.call(ops::OBSERVE, json!({})).await {
        Ok(obs) => {
            let w = obs["window"].as_str().unwrap_or_default();
            if !w.contains("brave") {
                gaps.push(format!(
                    "the Brave lane's own session observed {w:?} instead of its lane's window"
                ));
            }
        }
        Err(e) => gaps.push(format!(
            "the Brave lane's own session could not observe its lane: {} ({}){}",
            e.code,
            e.message,
            foreground_note(&e)
        )),
    }

    // The other lane's session must not see it.
    match two.call(ops::OBSERVE, json!({})).await {
        Ok(obs) => gaps.push(format!(
            "a session in the Vivaldi lane read the Brave lane's window {:?} (url {:?}); \
             windows of other lanes must be invisible",
            obs["window"].as_str().unwrap_or_default(),
            obs["url"].as_str().unwrap_or_default()
        )),
        // Three honest ways to fail, depending on what is on the machine:
        // the window is another lane's (no_target), the lane's browser is
        // not there at all (lane_unavailable), or the lane is real and
        // nothing is attached that can read it (no_backend -- its own
        // extension is absent and no UIA backend is connected in this test).
        // What must never happen is the fourth outcome: an answer.
        Err(e)
            if e.code == codes::NO_TARGET
                || e.code == "lane_unavailable"
                || e.code == codes::NO_BACKEND => {}
        Err(e) => gaps.push(format!(
            "cross-lane observe failed with {} ({}), which is none of the expected refusals{}",
            e.code,
            e.message,
            foreground_note(&e)
        )),
    }

    brave.task.abort();
    report("lane-scoped targeting", gaps);
}

// ---------------------------------------------------------------------------
// (c) env/pipe mismatch gives no_backend
// ---------------------------------------------------------------------------

/// Spec 5: one broker per OS user, `KRATOS_PIPE` a test override only.
///
/// What this reproduced in P0: the pipe name *was* the pairing mechanism, so a
/// mismatch put the extension on one broker and the agent on another, and the
/// agent was told `no_backend` -- which named the wrong cause.
///
/// Settled in P3 three ways: the single-instance guard is keyed on the OS user
/// rather than on the pipe, so the only way to get a second broker is the test
/// seam this very test uses; an unreachable lane says `lane_unavailable`
/// rather than `no_backend`; and `status` reports every session's lane, so a
/// split is visible rather than inferred.
#[tokio::test]
async fn a_stranded_session_is_told_which_lane_it_cannot_reach() {
    let with_backend = Harness::start("splitA");
    let without = Harness::start("splitB");
    wait_for_pipe(&with_backend.pipe).await;
    wait_for_pipe(&without.pipe).await;

    let brave = FakeInstance::dom_ext("brave", "tok-brave").spawn(with_backend.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    let mut ui = settings(&without.pipe).await;
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": "opera", "approved": true}],
        }}),
    )
    .await
    .expect("config.set");

    let mut stranded = session(&without.pipe, "claude", None, "s-1").await;
    let mut gaps = Vec::new();

    // The lane is visible in `status`, so "which browser is this session
    // pointed at" is answerable without guessing.
    let status = stranded.call(ops::STATUS, json!({})).await.expect("status");
    match status["lanes"].as_array() {
        Some(lanes) if !lanes.is_empty() => {
            let mine = lanes.iter().find(|l| l["peer_id"] == json!(stranded.peer_id));
            match mine {
                Some(l) => {
                    if l["lane"]["browser"] != json!("opera") {
                        gaps.push(format!("status names the wrong lane: {}", l["lane"]));
                    }
                }
                None => gaps.push("status lists lanes but not this session's".to_string()),
            }
        }
        _ => gaps.push("status reports no `lanes`, so a split is not diagnosable".to_string()),
    }

    // And an op against a lane that cannot be reached names the lane, not the
    // backend.
    match stranded.call(ops::OBSERVE, json!({})).await {
        Ok(obs) => gaps.push(format!("an unreachable lane answered anyway: {obs}")),
        Err(e) if e.code == "lane_unavailable" => {
            if !e.message.contains("opera") {
                gaps.push(format!("lane_unavailable without naming the lane: {}", e.message));
            }
        }
        Err(e) => gaps.push(format!(
            "an unreachable browser lane reports {} ({}); the spec's code for it is \
             lane_unavailable",
            e.code, e.message
        )),
    }

    brave.task.abort();
    report("an unreachable lane", gaps);
}

// ---------------------------------------------------------------------------
// (d) two brokers hold two leases
// ---------------------------------------------------------------------------

/// Spec 5: native and physical actions share **one machine-wide** input lease,
/// because there is one mouse and one keyboard. The lease lives in broker
/// state and the single-instance mutex is keyed to the pipe name
/// (`kratos_ipc::instance_name`), so a second broker on a second pipe grants a
/// second lease and two agents drive one pointer at once.
#[tokio::test]
async fn only_one_input_lease_exists_for_one_os_user() {
    let first = Harness::start_sharing_input("leaseA");
    let second = Harness::start_sharing_input("leaseB");
    wait_for_pipe(&first.pipe).await;
    wait_for_pipe(&second.pipe).await;

    let mut a = session(&first.pipe, "harness-one", None, "s-1").await;
    let mut b = session(&second.pipe, "harness-two", None, "s-2").await;

    let want = json!({"scope": "input", "ttl_ms": 4000, "wait_ms": 0});
    let one = a.call(ops::LEASE_ACQUIRE, want.clone()).await;
    let two = b.call(ops::LEASE_ACQUIRE, want).await;

    let mut gaps = Vec::new();
    match (&one, &two) {
        (Ok(x), Ok(y)) => gaps.push(format!(
            "two brokers for one OS user each granted the input lease ({} and {}); there is one \
             mouse, so there must be one lease",
            x["token"].as_str().unwrap_or_default(),
            y["token"].as_str().unwrap_or_default()
        )),
        (Err(e), Err(f)) => {
            gaps.push(format!("neither agent could lease input: {} / {}", e.code, f.code))
        }
        _ => {}
    }
    if let Err(e) = &two {
        if e.code != codes::LEASE_HELD {
            gaps.push(format!("the second request failed {} rather than lease_held", e.code));
        }
    }

    report("one machine-wide input lease", gaps);
}

// ---------------------------------------------------------------------------
// P2 acceptance: identity, lanes, approval
// ---------------------------------------------------------------------------

/// Spec 3: the lane is in the hello reply, so a session knows where it is
/// before it does anything.
#[tokio::test]
async fn the_hello_reply_carries_the_lane_and_the_approved_flag() {
    let h = Harness::start("hellolane");
    wait_for_pipe(&h.pipe).await;
    let one = session(&h.pipe, "claude", Some("agent-a"), "s-1").await;

    let lane = one.ack.get("lane").expect("a lane in the hello reply");
    assert_eq!(one.ack["approved"], json!(true), "the config approves this harness");
    assert_eq!(lane["assigned"]["harness"], json!("claude"));
    assert_eq!(lane["assigned"]["agent"], json!("agent-a"));
    assert!(lane["browser"].is_string(), "{lane}");
    assert!(lane["profile_dir"].is_string(), "{lane}");
    assert!(
        ["ready", "launching", "degraded", "unavailable"]
            .contains(&lane["state"].as_str().unwrap_or("")),
        "state must be one of the four: {lane}"
    );

    // A backend has no lane, and the settings window does not have one either:
    // neither drives a browser on an agent's behalf.
    let ui = settings(&h.pipe).await;
    assert!(ui.ack.get("lane").is_none() || ui.ack["lane"].is_null(), "{}", ui.ack);
    assert_eq!(ui.ack["approved"], json!(true), "the settings window is the user");
}

/// Spec 7: `whoami` is how an agent asks where it is -- and the answer is
/// re-resolved, so a browser allocated mid-session is visible without
/// reconnecting.
#[tokio::test]
async fn whoami_answers_with_the_lane_and_follows_the_users_allocation() {
    let h = Harness::start("whoami");
    wait_for_pipe(&h.pipe).await;
    let mut one = session(&h.pipe, "claude", Some("agent-a"), "s-1").await;

    let me = one.call("whoami", json!({})).await.expect("whoami");
    assert_eq!(me["harness"], json!("claude"));
    assert_eq!(me["agent"], json!("agent-a"));
    assert_eq!(me["session"], json!("s-1"));
    assert_eq!(me["approved"], json!(true));
    let before = me["lane"]["browser"].as_str().unwrap_or_default().to_string();

    // The user picks a different browser for this harness.
    let other = ["chrome", "edge", "vivaldi", "brave"]
        .into_iter()
        .find(|b| *b != before)
        .expect("something to switch to");
    let mut ui = settings(&h.pipe).await;
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {"harnesses": [{"id": "claude", "browser": other, "approved": true}]}}),
    )
    .await
    .expect("config.set");

    let after = one.call("whoami", json!({})).await.expect("whoami again");
    assert_eq!(
        after["lane"]["browser"], json!(other),
        "whoami must answer from the config as it is now, not from the handshake"
    );
}

/// Spec 3: an unknown harness is listed for the user and refused everything
/// but asking who it is. The dialog is raised; nothing waits on it.
#[tokio::test]
async fn an_unapproved_harness_may_only_ask_who_it_is() {
    let h = Harness::start("unapproved");
    wait_for_pipe(&h.pipe).await;
    // A name deliberately absent from this suite's approved list.
    let mut stranger = session(&h.pipe, "some-other-client", None, "s-9").await;
    assert_eq!(stranger.ack["approved"], json!(false));

    // Two ops are allowed, because a client that cannot ask why it is refused
    // leaves its operator with nothing to go on.
    assert!(stranger.call(ops::STATUS, json!({})).await.is_ok());
    let me = stranger.call("whoami", json!({})).await.expect("whoami");
    assert_eq!(me["approved"], json!(false));

    // Everything else is refused, and quickly: a blocking approval once cost
    // 241 of 272 seconds of a session.
    let started = std::time::Instant::now();
    let err = stranger.call(ops::OBSERVE, json!({})).await.unwrap_err();
    assert_eq!(err.code, "not_approved");
    assert!(err.retry_safe, "the retry is what succeeds once a human clicks");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "an unapproved op must not sit on the dialog: {:?}",
        started.elapsed()
    );
    let err = stranger
        .call(ops::ACT, json!({"instruction": "click the \"Send\" button"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, "not_approved");

    // And the user can find it in the settings window to approve it.
    let mut ui = settings(&h.pipe).await;
    let listed = ui.call("harness.list", json!({})).await.expect("harness.list");
    let found = listed["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == json!("some-other-client"))
        .expect("an unknown harness must be listed, or there is nothing to approve");
    assert_eq!(found["approved"], json!(false));
    assert_eq!(found["browser"], json!(""), "unassigned until the user says");
}

/// Spec 3: "every new session re-resolves; nothing persists per session".
#[tokio::test]
async fn a_new_session_resolves_again_from_the_config() {
    let h = Harness::start("resolveagain");
    wait_for_pipe(&h.pipe).await;

    let first = session(&h.pipe, "claude", None, "s-1").await;
    let before = first.ack["lane"]["browser"].as_str().unwrap_or_default().to_string();

    let other = ["chrome", "edge", "vivaldi", "brave"]
        .into_iter()
        .find(|b| *b != before)
        .expect("something to switch to");
    let mut ui = settings(&h.pipe).await;
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {"harnesses": [{"id": "claude", "browser": other, "approved": true}]}}),
    )
    .await
    .expect("config.set");

    let second = session(&h.pipe, "claude", None, "s-2").await;
    assert_eq!(second.ack["lane"]["browser"], json!(other));
    // The first session's handshake answer is not retroactively rewritten --
    // it said what was true when it connected.
    assert_eq!(first.ack["lane"]["browser"], json!(before));
}

/// Spec 3: an agent's assigned profile is used when it exists, and a missing
/// one falls back with a warning rather than opening a profile with none of
/// the user's logins in it.
#[tokio::test]
async fn an_agents_profile_is_used_and_a_missing_one_warns() {
    let h = Harness::start("profiles");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;

    // Which browsers and profiles this machine actually has -- the same answer
    // the settings window's dropdown is filled from.
    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let browsers = lanes["browsers"].as_array().cloned().unwrap_or_default();
    let Some(browser) = browsers.iter().find(|b| b["installed"] == json!(true)) else {
        eprintln!("no Chromium browser on this machine; profile resolution untested");
        return;
    };
    let brand = browser["brand"].as_str().unwrap().to_string();
    let profiles = browser["profiles"].as_array().cloned().unwrap_or_default();

    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": brand, "approved": true}],
            "agents": [{"id": "agent-a", "profile_dir": "Profile does not exist 42"}],
        }}),
    )
    .await
    .expect("config.set");

    let one = session(&h.pipe, "claude", Some("agent-a"), "s-1").await;
    let lane = &one.ack["lane"];
    if profiles.is_empty() {
        eprintln!("{brand} lists no profiles here; nothing to contradict");
        return;
    }
    assert_eq!(
        lane["profile_dir"], browser["last_used"],
        "a profile that is not there must fall back to the last-used one: {lane}"
    );
    let warning = lane["warning"].as_str().expect("a fallback must be said out loud");
    assert!(warning.contains("Profile does not exist 42"), "{warning}");

    // And a profile that does exist is used as given.
    let real = profiles[profiles.len() - 1]["dir"].as_str().unwrap().to_string();
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {"agents": [{"id": "agent-a", "profile_dir": real}]}}),
    )
    .await
    .expect("config.set");
    let two = session(&h.pipe, "claude", Some("agent-a"), "s-2").await;
    assert_eq!(two.ack["lane"]["profile_dir"], json!(real));
    assert!(two.ack["lane"]["warning"].is_null(), "{}", two.ack["lane"]);
}

/// Spec 7 and 8: the settings window pairs an instance token with a profile,
/// and that is what turns a lane from degraded into ready.
#[tokio::test]
async fn pairing_an_instance_with_a_profile_makes_the_lane_ready() {
    let h = Harness::start("pairing");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;

    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let Some(browser) = lanes["browsers"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["installed"] == json!(true)))
        .cloned()
    else {
        eprintln!("no Chromium browser on this machine; pairing untested");
        return;
    };
    let brand = browser["brand"].as_str().unwrap().to_string();
    let profile = browser["last_used"].as_str().unwrap().to_string();

    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {"harnesses": [{"id": "claude", "browser": brand, "approved": true}]}}),
    )
    .await
    .expect("config.set");

    let instance = FakeInstance::dom_ext("brave", "tok-pair").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    // Attached but unpaired: the extension cannot report a profile directory,
    // so until somebody says which profile it is, the lane is degraded.
    let before = session(&h.pipe, "claude", None, "s-1").await;
    assert_eq!(before.ack["lane"]["state"], json!("degraded"));

    // The instance is listed for pairing, unbound.
    let listed = ui.call("lane.list", json!({})).await.expect("lane.list");
    let found = listed["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["token"] == json!("tok-pair"))
        .expect("an attached instance must be listed");
    assert!(found["bound_to"].is_null());

    ui.call(
        "instance.bind",
        json!({"token": "tok-pair", "brand": brand, "profile_dir": profile}),
    )
    .await
    .expect("instance.bind");

    let after = session(&h.pipe, "claude", None, "s-2").await;
    assert_eq!(
        after.ack["lane"]["state"], json!("ready"),
        "a paired instance for this browser and profile is the definition of ready: {}",
        after.ack["lane"]
    );

    // Unpairing is the same op with nothing to pair to.
    ui.call("instance.bind", json!({"token": "tok-pair"})).await.expect("unbind");
    let undone = session(&h.pipe, "claude", None, "s-3").await;
    assert_eq!(undone.ack["lane"]["state"], json!("degraded"));

    instance.task.abort();
}

/// An agent cannot allocate itself anything, whatever it claims.
#[tokio::test]
async fn an_agent_cannot_allocate_or_approve_itself() {
    let h = Harness::start("noselfserve");
    wait_for_pipe(&h.pipe).await;
    let mut one = session(&h.pipe, "claude", Some("agent-a"), "s-1").await;

    // The ops that would let it: all of them are `ui` only.
    for op in ["config.set", "config.get", "instance.bind", "lane.list", "harness.list",
               "agent.list", "kill.reset", "shutdown"] {
        let err = one.call(op, json!({"patch": {}})).await.unwrap_err();
        assert_eq!(err.code, codes::UNAUTHORIZED, "an agent must not be able to call {op}");
    }
}

// ---------------------------------------------------------------------------
// P3 acceptance: parallel lanes, tab ownership, the pause
// ---------------------------------------------------------------------------

/// Two installed Chromium brands, or `None` on a machine with fewer.
async fn two_brands(ui: &mut kratos_ipc::PipePeer) -> Option<(String, String)> {
    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let installed: Vec<String> = lanes["browsers"]
        .as_array()?
        .iter()
        .filter(|b| b["installed"] == json!(true))
        .filter_map(|b| b["brand"].as_str().map(|s| s.to_string()))
        .collect();
    match installed.len() {
        0 | 1 => None,
        _ => Some((installed[0].clone(), installed[1].clone())),
    }
}

/// Spec 5: "DOM actions need no input lease and run in parallel across lanes
/// and sessions."
///
/// Proved by holding the lease rather than by timing: a third session takes
/// the input devices and keeps them, and two DOM sessions act anyway. A timing
/// test would prove the same thing less certainly and fail on a slow machine.
#[tokio::test]
async fn two_sessions_on_two_lanes_act_while_the_input_lease_is_held() {
    let h = Harness::start("parallel");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    let Some((one_brand, two_brand)) = two_brands(&mut ui).await else {
        eprintln!("fewer than two Chromium browsers here; parallel lanes untested");
        return;
    };

    let a = FakeInstance::dom_ext_brand(&one_brand, "tok-a").spawn(h.pipe.clone());
    let b = FakeInstance::dom_ext_brand(&two_brand, "tok-b").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            // A whole-array patch replaces the list, so the lease holder
            // has to be in it too -- an approval dropped here is a dialog on
            // somebody's screen during a test run.
            "harnesses": [
                {"id": "claude", "browser": one_brand, "approved": true},
                {"id": "bluee", "browser": two_brand, "approved": true},
                {"id": "harness-one", "browser": "none", "approved": true},
            ],
            "instances": [
                {"token": "tok-a", "brand": one_brand, "profile_dir": "Default"},
                {"token": "tok-b", "brand": two_brand, "profile_dir": "Default"},
            ],
            "defaults": {"profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    // A third session holds the mouse and keyboard for the whole test.
    let mut holder = session(&h.pipe, "harness-one", None, "s-hold").await;
    holder
        .call(ops::LEASE_ACQUIRE, json!({"scope": "input", "ttl_ms": 30000, "wait_ms": 0}))
        .await
        .expect("the holder takes the input devices");

    let mut one = session(&h.pipe, "claude", None, "s-1").await;
    let mut two = session(&h.pipe, "bluee", None, "s-2").await;
    assert_eq!(one.ack["lane"]["state"], json!("ready"), "{}", one.ack["lane"]);
    assert_eq!(two.ack["lane"]["state"], json!("ready"), "{}", two.ack["lane"]);

    let first = one.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await;
    let second = two.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await;

    for (who, res) in [("claude", &first), ("bluee", &second)] {
        let r = res.as_ref().unwrap_or_else(|e| {
            panic!("{who} could not act through its own lane while the lease was held: {} {}",
                   e.code, e.message)
        });
        assert_ne!(r["status"], json!("paused"), "{who}: {r}");
        assert_eq!(r["backend"], json!("dom-ext"), "{who} must go through its own instance: {r}");
    }

    // Each instance acted once: the lanes did not cross.
    tokio::time::sleep(SETTLE).await;
    assert_eq!(a.acted.load(Ordering::SeqCst), 1, "the first lane's instance");
    assert_eq!(b.acted.load(Ordering::SeqCst), 1, "the second lane's instance");

    a.task.abort();
    b.task.abort();
}

/// Spec 5: a tab belongs to the session that got there first, unless the user
/// marks the lane shared.
#[tokio::test]
async fn a_tab_belongs_to_the_session_that_acted_on_it_first() {
    let h = Harness::start("tabowner");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let Some(brand) = lanes["browsers"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["installed"] == json!(true)))
        .and_then(|b| b["brand"].as_str().map(|s| s.to_string()))
    else {
        eprintln!("no Chromium browser here; tab ownership untested");
        return;
    };

    let inst = FakeInstance::dom_ext_brand(&brand, "tok-one").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": brand, "approved": true}],
            "instances": [{"token": "tok-one", "brand": brand, "profile_dir": "Default"}],
            "defaults": {"profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    // Two sessions of one harness: one lane, one profile, one set of tabs.
    let mut first = session(&h.pipe, "claude", None, "s-1").await;
    let mut second = session(&h.pipe, "claude", None, "s-2").await;

    // Dispatched is what matters here, not what the pixels did: a fake
    // backend changes nothing on screen, so `no_effect` is the honest verdict
    // and the question under test is whether it ran at all.
    let mine = first
        .call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"}))
        .await
        .expect("the first session acts");
    assert!(ran(&mine), "{mine}");

    let theirs = second
        .call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"}))
        .await
        .expect("a refusal is a result, not an error");
    assert_eq!(theirs["status"], json!("blocked"), "{theirs}");
    assert_eq!(theirs["gate"]["category"], json!("tab_owned"));
    assert!(
        theirs["gate"]["rule"].as_str().unwrap_or_default().contains(&first.peer_id),
        "the refusal must name the owner: {theirs}"
    );

    // The user marks the lane shared, and they share it.
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {"lanes": [{"browser": brand, "profile_dir": "Default", "shared": true}]}}),
    )
    .await
    .expect("config.set");
    let now = second
        .call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"}))
        .await
        .expect("act");
    assert!(ran(&now), "a shared lane lets both sessions act: {now}");

    // And a session that goes away stops owning anything.
    let owner_id = first.peer_id.clone();
    drop(first);
    tokio::time::sleep(SETTLE).await;
    ui.call(ops::CONFIG_SET, json!({"patch": {"lanes": []}})).await.expect("config.set");
    let mut third = session(&h.pipe, "claude", None, "s-3").await;
    let after = third
        .call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"}))
        .await
        .expect("act");
    assert!(
        ran(&after),
        "a disconnected session ({owner_id}) must not own its tabs forever: {after}"
    );

    inst.task.abort();
}

// ---------------------------------------------------------------------------
// P5 acceptance: the ensure-lane ladder
// ---------------------------------------------------------------------------

/// Spec 4: a lane with nothing attached is opened, and the rungs that *refuse*
/// to open one are the ones a test can prove without starting a browser on
/// somebody's desktop.
///
/// The launch itself is verified live, on a real machine, with real browsers —
/// see PHASE_NOTES. What is pinned here is every way the ladder declines, and
/// that it declines rather than doing something surprising.
#[tokio::test]
async fn a_lane_that_is_not_installed_cannot_be_opened_and_says_which() {
    let h = Harness::start("ladderabsent");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    // Opera is not on this machine, and the point of the test is the message.
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": "opera", "approved": true}],
            "defaults": {"open_method": "direct", "profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    let mut one = session(&h.pipe, "claude", None, "s-1").await;
    let err = one.call(ops::OBSERVE, json!({})).await.unwrap_err();
    assert_eq!(err.code, "lane_unavailable");
    assert!(err.message.contains("opera"), "{}", err.message);
    assert_eq!(err.detail["profile_dir"], json!("Default"), "{:?}", err.detail);
}

/// `open_method: "none"` is an allocation, not an omission: drive a browser
/// that is open, never open one.
#[tokio::test]
async fn the_operator_can_forbid_opening_a_browser_at_all() {
    let h = Harness::start("ladderoff");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let Some(brand) = lanes["browsers"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["installed"] == json!(true)))
        .and_then(|b| b["brand"].as_str().map(|s| s.to_string()))
    else {
        eprintln!("no Chromium browser here; nothing to decline to open");
        return;
    };
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": brand, "approved": true}],
            // Already the suite's default; set here so the test states what
            // it is testing rather than relying on the harness.
            "defaults": {"open_method": "none", "profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    let mut one = session(&h.pipe, "claude", None, "s-1").await;
    // Whatever the outcome, it must not be "a browser was started". With no
    // instance paired the lane is degraded, and an observe either reaches a
    // window of that brand through UIA or reports there is no backend for it.
    match one.call(ops::OBSERVE, json!({})).await {
        Ok(obs) => assert!(obs["elements"].is_array(), "{obs}"),
        Err(e) => assert!(
            [codes::NO_BACKEND, codes::NO_TARGET, "lane_unavailable"].contains(&e.code.as_str()),
            "unexpected refusal {} ({})",
            e.code,
            e.message
        ),
    }
}

/// The kill switch stops the ladder before it starts a process.
///
/// `observe` is the case that matters: it is read-only and therefore never
/// went through the kill check, and before the ladder existed it could not
/// start anything either. Now it can, so it has to ask.
#[tokio::test]
async fn an_engaged_kill_switch_opens_no_browser() {
    let h = Harness::start("ladderkill");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let Some(brand) = lanes["browsers"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["installed"] == json!(true)))
        .and_then(|b| b["brand"].as_str().map(|s| s.to_string()))
    else {
        eprintln!("no Chromium browser here; skipping");
        return;
    };
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": brand, "approved": true}],
            "defaults": {"open_method": "direct", "profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    let mut one = session(&h.pipe, "claude", None, "s-1").await;
    one.call(ops::KILL_ENGAGE, json!({"scope": "global", "reason": "test"}))
        .await
        .expect("an agent may engage the kill switch");

    let err = one.call(ops::OBSERVE, json!({})).await.unwrap_err();
    assert_eq!(
        err.code,
        codes::KILL_SWITCH,
        "a halted system must not start a browser: {} {}",
        err.code,
        err.message
    );
    assert!(err.message.contains("no browser will be opened"), "{}", err.message);
}

/// A lane with its instance already paired is rung one: nothing is opened.
#[tokio::test]
async fn a_lane_that_is_already_attached_opens_nothing() {
    let h = Harness::start("ladderattached");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    let lanes = ui.call("lane.list", json!({})).await.expect("lane.list");
    let Some(brand) = lanes["browsers"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["installed"] == json!(true)))
        .and_then(|b| b["brand"].as_str().map(|s| s.to_string()))
    else {
        eprintln!("no Chromium browser here; skipping");
        return;
    };

    let instance = FakeInstance::dom_ext_brand(&brand, "tok-ready").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [{"id": "claude", "browser": brand, "approved": true}],
            "instances": [{"token": "tok-ready", "brand": brand, "profile_dir": "Default"}],
            // Opening is allowed, and must not happen: the rung above it
            // already matched.
            "defaults": {"open_method": "direct", "profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    let mut one = session(&h.pipe, "claude", None, "s-1").await;
    assert_eq!(one.ack["lane"]["state"], json!("ready"), "{}", one.ack["lane"]);
    let obs = one.call(ops::OBSERVE, json!({})).await.expect("observe");
    assert_eq!(obs["source"], json!("dom-ext"));
    assert!(
        obs["window"].as_str().unwrap_or_default().contains(&brand),
        "the attached instance should have answered: {obs}"
    );
    instance.task.abort();
}

// ---------------------------------------------------------------------------
// P7: the whole matrix
// ---------------------------------------------------------------------------

/// Two agents, two instances, every kill scope, pause on and off.
///
/// The spec's P7 acceptance item. Every cell is also covered by a test of its
/// own above; the cross product is here because that is where the mistakes
/// live. A kill scope that stops the wrong lane and a pause that stops a DOM
/// action are both invisible to a test that varies one thing at a time.
///
/// What this cannot do is make the *user* type. Pause fires on real keyboard
/// and mouse input, and input this process injects is ignored on purpose --
/// that is the whole point of the `LLMHF_INJECTED` check -- so no test outside
/// the broker's own process can produce the condition. The paused branch is
/// covered by `activity::tests` in the broker; what the matrix checks is that
/// the setting is honoured, reported, and does not stop a DOM action.
#[tokio::test]
async fn the_matrix_two_agents_two_instances_kill_scopes_and_pause() {
    let h = Harness::start("matrix");
    wait_for_pipe(&h.pipe).await;
    let mut ui = settings(&h.pipe).await;
    let Some((brand_a, brand_b)) = two_brands(&mut ui).await else {
        eprintln!("fewer than two Chromium browsers here; the matrix needs two lanes");
        return;
    };

    let _a = FakeInstance::dom_ext_brand(&brand_a, "tok-a").spawn(h.pipe.clone());
    let _b = FakeInstance::dom_ext_brand(&brand_b, "tok-b").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    // Two harnesses, one browser each; two agents, one profile each.
    ui.call(
        ops::CONFIG_SET,
        json!({"patch": {
            "harnesses": [
                {"id": "claude", "browser": brand_a, "approved": true},
                {"id": "bluee", "browser": brand_b, "approved": true},
            ],
            "agents": [
                {"id": "agent-one", "profile_dir": "Default"},
                {"id": "agent-two", "profile_dir": "Default"},
            ],
            "instances": [
                {"token": "tok-a", "brand": brand_a, "profile_dir": "Default"},
                {"token": "tok-b", "brand": brand_b, "profile_dir": "Default"},
            ],
            "defaults": {"profile": "Default"},
        }}),
    )
    .await
    .expect("config.set");

    let mut one = session(&h.pipe, "claude", Some("agent-one"), "s-1").await;
    let mut two = session(&h.pipe, "bluee", Some("agent-two"), "s-2").await;
    assert_eq!(one.ack["lane"]["state"], json!("ready"), "{}", one.ack["lane"]);
    assert_eq!(two.ack["lane"]["state"], json!("ready"), "{}", two.ack["lane"]);
    assert_eq!(one.ack["lane"]["browser"], json!(brand_a), "{}", one.ack["lane"]);
    assert_eq!(two.ack["lane"]["browser"], json!(brand_b), "{}", two.ack["lane"]);

    let mut failures: Vec<String> = Vec::new();

    for pause in [false, true] {
        ui.call(
            ops::CONFIG_SET,
            json!({"patch": {"defaults": {
                "pause_on_user_input": pause,
                // Short, so that if anybody *is* at this machine the wait is
                // a moment rather than half a minute.
                "user_idle_ms": 300,
                "pause_max_ms": 1500,
            }}}),
        )
        .await
        .expect("config.set pause");

        let status = ui.call(ops::STATUS, json!({})).await.expect("status");
        if status["pause"]["on_user_input"] != json!(pause) {
            failures.push(format!(
                "pause={pause}: status does not report the setting: {}",
                status["pause"]
            ));
        }

        // scope -> which lanes it should stop
        let scopes: Vec<(String, bool, bool)> = vec![
            ("".to_string(), false, false),
            (format!("browser:{brand_a}"), true, false),
            (format!("browser:{brand_b}"), false, true),
            ("global".to_string(), true, true),
        ];

        for (scope, stops_one, stops_two) in scopes {
            if !scope.is_empty() {
                ui.call(
                    ops::KILL_ENGAGE,
                    json!({"scope": scope, "reason": "matrix"}),
                )
                .await
                .expect("kill.engage");
            }

            for (who, peer, stopped) in [
                ("claude", &mut one, stops_one),
                ("bluee", &mut two, stops_two),
            ] {
                let res = peer.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await;
                let label = format!("pause={pause} scope={scope:?} {who}");
                match (&res, stopped) {
                    // Stopped: either the kill switch refused it outright or
                    // the act came back `killed`. Both are honest; what must
                    // never happen is that it ran.
                    (Ok(v), true) => {
                        if v["status"] != json!("killed") {
                            failures.push(format!("{label}: ran anyway: {v}"));
                        }
                    }
                    (Err(e), true) => {
                        if e.code != codes::KILL_SWITCH {
                            failures.push(format!("{label}: {} {}", e.code, e.message));
                        }
                    }
                    // Not stopped: it must go through its own instance, and
                    // a DOM action must not be paused -- it never touches
                    // the pointer, so there is nothing for the user to
                    // collide with.
                    (Ok(v), false) => {
                        if v["status"] == json!("killed") {
                            failures.push(format!("{label}: killed by another lane's scope: {v}"));
                        }
                        if v["status"] == json!("paused") {
                            failures.push(format!(
                                "{label}: a DOM action was paused; it needs no pointer: {v}"
                            ));
                        }
                        if v["backend"] != json!("dom-ext") {
                            failures.push(format!("{label}: wrong backend: {v}"));
                        }
                    }
                    (Err(e), false) => {
                        failures.push(format!("{label}: refused: {} {}", e.code, e.message));
                    }
                }
            }

            if !scope.is_empty() {
                // Only the settings window can clear one. An agent asking is
                // the thing that must be refused, so ask as the agent first.
                let refused = one.call(ops::KILL_RESET, json!({"scope": scope})).await;
                if refused.is_ok() {
                    failures.push(format!("an agent cleared the kill switch for {scope:?}"));
                }
                ui.call(ops::KILL_RESET, json!({"scope": scope})).await.expect("kill.reset");
            }
        }
    }

    eprintln!(
        "P7 matrix: {brand_a} and {brand_b}, 2 pause settings x 4 kill scopes x 2 lanes, 16 cells"
    );
    report("P7 matrix: two agents x two instances x kill scopes x pause", failures);
}
