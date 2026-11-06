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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kratos_ipc::{FrameReader, FrameWriter};
use kratos_protocol::{codes, ops, Frame, Role};
use serde_json::{json, Value};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::windows::named_pipe::NamedPipeClient;

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
        let unique = format!("kratos-p0-{}-{}", std::process::id(), label);
        let dir = std::env::temp_dir().join(&unique);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = json!({
            "idle_exit_ms": 0,
            "decider": {"provider": "mock", "model": "test"},
            "kill": {"hotkey": "ctrl+alt+shift+f23", "hotkey_scope": "global"},
            "gate": {
                "block_keywords": [], "review_keywords": [], "review_apps": [],
                "review_domains": [], "rate_limit_per_minute": 100,
                "require_os_dialog_for": [], "confirm_ttl_ms": 60000
            },
            "thresholds": {"default": 0.0, "click": 0.0, "type": 0.0, "key": 0.0,
                           "scroll": 0.0, "select": 0.0, "navigate": 0.0,
                           "open": 0.0, "focus": 0.0}
        });
        let config_path = dir.join("config.json");
        let mut f = std::fs::File::create(&config_path).expect("config");
        f.write_all(serde_json::to_string_pretty(&config).unwrap().as_bytes()).unwrap();
        drop(f);

        let child = Command::new(env!("CARGO_BIN_EXE_kratos-broker"))
            .env("KRATOS_PIPE", &unique)
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
// A raw peer, so a hello can carry fields this build does not know about
// ---------------------------------------------------------------------------

/// `PipePeer::handshake` builds the hello payload itself, so it cannot send
/// `harness`, `agent`, `session` or `instance`. This sends the frame directly.
/// Protocol rule 3 says unknown fields must be ignored, so the shape being
/// accepted proves nothing by itself -- what matters is what comes back.
struct RawPeer {
    reader: FrameReader<ReadHalf<NamedPipeClient>>,
    writer: FrameWriter<WriteHalf<NamedPipeClient>>,
    seq: u64,
    pub ack: Value,
}

impl RawPeer {
    async fn hello(pipe: &str, mut payload: Value) -> RawPeer {
        let conn = kratos_ipc::connect(pipe).await.expect("connect");
        let (reader, writer) = kratos_ipc::split(conn);
        let mut p = RawPeer { reader, writer, seq: 0, ack: Value::Null };
        payload["build"] = json!(BUILD);
        payload["v_min"] = json!(kratos_protocol::V);
        payload["v_max"] = json!(kratos_protocol::V);
        p.ack = p.call(ops::HELLO, payload).await.expect("hello");
        p
    }

    async fn agent(pipe: &str, harness: &str, agent: &str, session: &str) -> RawPeer {
        RawPeer::hello(
            pipe,
            json!({
                "role": "agent",
                "name": harness,
                "harness": harness,
                "agent": agent,
                "session": session,
            }),
        )
        .await
    }

    async fn call(&mut self, op: &str, payload: Value) -> Result<Value, kratos_protocol::WireError> {
        self.seq += 1;
        let id = format!("r{}", self.seq);
        self.writer.send(&Frame::req(&id, op, payload)).await.expect("send");
        loop {
            let frame = match self.reader.next().await {
                Ok(Some(f)) => f,
                Ok(None) => panic!("broker closed the connection"),
                Err(e) if e.is_fatal() => panic!("connection lost: {e}"),
                Err(_) => continue,
            };
            if matches!(frame.body, kratos_protocol::Body::Evt { .. }) {
                continue;
            }
            if frame.id() != Some(id.as_str()) {
                continue;
            }
            return if frame.is_ok() {
                Ok(frame.payload().clone())
            } else {
                Err(frame.wire_error().cloned().expect("error body"))
            };
        }
    }
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
    source: &'static str,
    brand: &'static str,
    /// The instance token the spec has the extension generate per profile.
    token: &'static str,
    profile_dir: &'static str,
    window: String,
    url: String,
}

struct Live {
    /// Set when the broker sends `evt superseded` to this instance.
    superseded: Arc<AtomicBool>,
    /// Set when the broker drops the connection.
    closed: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeInstance {
    fn dom_ext(brand: &'static str, token: &'static str, profile_dir: &'static str) -> Self {
        FakeInstance {
            source: "dom-ext",
            brand,
            token,
            profile_dir,
            window: format!("Inbox - Gmail - {brand}"),
            url: "https://mail.google.com/mail/u/0/#inbox".to_string(),
        }
    }

    fn elements(&self) -> Vec<Value> {
        vec![
            json!({"id": format!("{}:w1:t1:e0", self.brand), "role": "button", "label": "Send",
                   "container": "dialog:New Message", "bbox": [100, 200, 80, 24],
                   "selector": "div[role=dialog] button", "enabled": true}),
            json!({"id": format!("{}:w1:t1:e1", self.brand), "role": "link", "label": "Inbox",
                   "container": "nav:Mail", "bbox": [10, 60, 70, 20],
                   "selector": "nav a", "enabled": true}),
        ]
    }

    /// Attach with a hello that carries the instance object from section 7 of
    /// the spec. This build ignores it; a later one keys the registry on it.
    fn spawn(self, pipe: String) -> Live {
        let superseded = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let sup = superseded.clone();
        let cl = closed.clone();
        let task = tokio::spawn(async move {
            let mut peer = RawPeer::hello(
                &pipe,
                json!({
                    "role": "backend",
                    "name": format!("fake-{}-{}", self.source, self.brand),
                    "source": self.source,
                    "handles": ["browser"],
                    "instance": {
                        "token": self.token,
                        "brand": self.brand,
                        "profile_dir": self.profile_dir,
                        "pid": 4242,
                        "parent_hwnd": 0
                    }
                }),
            )
            .await;

            loop {
                let frame = match peer.reader.next().await {
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
                            .writer
                            .send(&Frame::ok(
                                &id,
                                json!({
                                    "window": self.window,
                                    "url": self.url,
                                    "elements": self.elements(),
                                    "truncated": false
                                }),
                            ))
                            .await;
                    }
                    ops::BACKEND_ACT => {
                        let _ = peer
                            .writer
                            .send(&Frame::ok(&id, json!({"applied": true, "took_ms": 3})))
                            .await;
                    }
                    _ => {}
                }
            }
        });
        Live { superseded, closed, task }
    }
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

/// Spec 7: the backend registry key becomes `(source, instance.token)` and
/// only the *same* key supersedes. Two browser profiles are two instances of
/// one source, and today the second one evicts the first, so one harness
/// opening its browser silently unplugs another's.
#[tokio::test]
#[ignore = "P0 reproduction: fixed in P1 (instance-keyed registration)"]
async fn two_browser_instances_both_stay_registered() {
    let h = Harness::start("twoinstances");
    wait_for_pipe(&h.pipe).await;

    let brave = FakeInstance::dom_ext("brave", "tok-brave", "Default").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;
    let vivaldi = FakeInstance::dom_ext("vivaldi", "tok-vivaldi", "Profile 1").spawn(h.pipe.clone());
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
    // The lane the agent is in decides which instance answers, so an
    // observation must be traceable to one instance. Today whichever peer the
    // map kept answers for everyone.
    if let Ok(obs) = a.call(ops::OBSERVE, json!({})).await {
        let answered = obs["window"].as_str().unwrap_or_default().to_string();
        if !answered.contains("brave") && !answered.contains("vivaldi") {
            gaps.push(format!("observe was answered by neither instance: window {answered:?}"));
        }
    }

    brave.task.abort();
    vivaldi.task.abort();
    report("two dom-ext instances", gaps);
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
#[ignore = "P0 reproduction: fixed in P2 (identity, lanes) and P3 (scoped targeting)"]
async fn a_session_observes_its_own_lane_not_another_agents_window() {
    let h = Harness::start("lanescope");
    wait_for_pipe(&h.pipe).await;

    // One attached instance, standing in for the Brave lane of harness
    // "claude". Harness "bluee" is allocated Vivaldi and has no instance.
    let brave = FakeInstance::dom_ext("brave", "tok-brave", "Default").spawn(h.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    let mut one = RawPeer::agent(&h.pipe, "claude", "agent-a", "s-1").await;
    let mut two = RawPeer::agent(&h.pipe, "bluee", "agent-b", "s-2").await;

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
        Err(e) if e.code == codes::NO_TARGET || e.code == "lane_unavailable" => {}
        Err(e) => gaps.push(format!(
            "cross-lane observe failed with {} ({}), expected no_target or lane_unavailable{}",
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

/// Spec 5: one broker per OS user, `KRATOS_PIPE` a test override only. Today
/// the pipe name is the pairing mechanism -- the browser, its native host and
/// the harness are matched by an inherited environment variable -- so a
/// mismatch puts the extension on one broker and the agent on another. The
/// agent is then told `no_backend`, which names the wrong cause: a backend
/// exists, it is simply plugged into a different broker. Nothing in the reply
/// or in `status` says so.
#[tokio::test]
#[ignore = "P0 reproduction: fixed in P1/P3 (instance binding, one broker, lane_unavailable)"]
async fn a_harness_and_its_browser_cannot_be_split_across_two_brokers() {
    let with_backend = Harness::start("splitA");
    let without = Harness::start("splitB");
    wait_for_pipe(&with_backend.pipe).await;
    wait_for_pipe(&without.pipe).await;

    // The browser was launched with one KRATOS_PIPE; the harness runs with
    // another. This is exactly what start-browser.cmd pairs today.
    let brave = FakeInstance::dom_ext("brave", "tok-brave", "Default").spawn(with_backend.pipe.clone());
    tokio::time::sleep(SETTLE).await;

    let mut stranded = agent(&without.pipe, "claude").await;
    let mut gaps = Vec::new();

    let status = stranded.call(ops::STATUS, json!({})).await.expect("status");
    if backend_sources(&status, "dom-ext") == 0 {
        gaps.push(
            "a second broker for the same OS user serves agents with no sight of the instance \
             attached to the first; one broker per OS user must make this impossible"
                .to_string(),
        );
    }
    if status.get("lanes").is_none() {
        gaps.push("status reports no `lanes`, so a split like this is not diagnosable".to_string());
    }

    match stranded.call(ops::OBSERVE, json!({})).await {
        Ok(obs) => {
            let w = obs["window"].as_str().unwrap_or_default();
            if !w.contains("brave") {
                gaps.push(format!("the stranded agent observed {w:?}, not its lane"));
            }
        }
        Err(e) if e.code == "lane_unavailable" => {}
        Err(e) if e.code == codes::NO_BACKEND => gaps.push(format!(
            "an unreachable browser lane reports no_backend ({}); the spec's code for it is \
             lane_unavailable, and no_backend names the wrong cause{}",
            e.message,
            foreground_note(&e)
        )),
        Err(e) => gaps.push(format!("observe failed {} ({})", e.code, e.message)),
    }

    brave.task.abort();
    report("one broker per OS user", gaps);
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
#[ignore = "P0 reproduction: fixed in P3 (one broker, machine-wide lease)"]
async fn only_one_input_lease_exists_for_one_os_user() {
    let first = Harness::start("leaseA");
    let second = Harness::start("leaseB");
    wait_for_pipe(&first.pipe).await;
    wait_for_pipe(&second.pipe).await;

    let mut a = agent(&first.pipe, "harness-one").await;
    let mut b = agent(&second.pipe, "harness-two").await;

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
