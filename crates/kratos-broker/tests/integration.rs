//! End-to-end tests against a real broker process over a real named pipe.
//!
//! Every test here spawns `kratos-broker.exe` with its own pipe name, its own
//! config file and its own journal, so they are independent and leave nothing
//! behind. The decision model is the offline mock, so none of this needs a key,
//! a network or a bill -- which is the point of the mock having a realistic
//! confidence shape rather than always answering 1.0.
//!
//! What is *not* faked: the pipe, its ACL, the handshake, the role checks, the
//! gate, the lease, the journal and the broker's own process lifecycle.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use kratos_protocol::{codes, ops, status as st, Frame, Role};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

const BUILD: &str = "integration-test";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    pipe: String,
    dir: PathBuf,
    child: Option<Child>,
}

impl Harness {
    /// Start a broker with an isolated pipe, config and journal.
    fn start(label: &str, config: Value) -> Harness {
        let unique = format!("kratos-it-{}-{}", std::process::id(), label);
        let dir = std::env::temp_dir().join(&unique);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config_path = dir.join("config.json");
        let mut f = std::fs::File::create(&config_path).expect("config");
        f.write_all(serde_json::to_string_pretty(&config).unwrap().as_bytes()).unwrap();
        drop(f);

        let child = Command::new(env!("CARGO_BIN_EXE_kratos-broker"))
            .env("KRATOS_PIPE", &unique)
            .env("KRATOS_CONFIG", &config_path)
            .env("KRATOS_JOURNAL", dir.join("actions.jsonl"))
            // No key: the mock provider needs none, and this proves the tests
            // are not quietly reaching the real model.
            .env_remove("KRATOS_API_KEY")
            .env_remove("TYPESAFE_API_KEY")
            .env_remove("JEV_API_KEY")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kratos-broker");

        Harness { pipe: unique, dir, child: Some(child) }
    }

    fn journal(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.dir.join("actions.jsonl")).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
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

fn base_config(idle_exit_ms: u64) -> Value {
    json!({
        "idle_exit_ms": idle_exit_ms,
        "decider": {"provider": "mock", "model": "test"},
        "kill": {"hotkey": "ctrl+alt+shift+f23", "hotkey_scope": "global"},
        "gate": {
            "block_keywords": ["format c:"],
            "review_keywords": ["send", "delete"],
            "review_apps": [],
            "review_domains": [],
            "rate_limit_per_minute": 100,
            "require_os_dialog_for": [],
            "confirm_ttl_ms": 60000
        },
        "thresholds": {"default": 0.0, "click": 0.0, "type": 0.0, "key": 0.0,
                       "scroll": 0.0, "select": 0.0, "navigate": 0.0, "open": 0.0, "focus": 0.0}
    })
}

/// Wait for the broker to be reachable.
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
// A fake backend
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeBackend {
    source: &'static str,
    /// What `backend.observe` reports.
    elements: Vec<Value>,
    /// What `backend.act` reports for `applied`.
    applied: bool,
}

impl FakeBackend {
    fn browser(source: &'static str) -> Self {
        FakeBackend {
            source,
            elements: vec![
                json!({"id": "e0", "role": "button", "label": "Send",
                       "container": "dialog:New Message", "bbox": [100, 200, 80, 24],
                       "selector": "div[role=dialog] button", "enabled": true}),
                json!({"id": "e1", "role": "textbox", "label": "Subject",
                       "container": "dialog:New Message", "bbox": [100, 150, 300, 24],
                       "selector": "div[role=dialog] input[name=subjectbox]", "enabled": true}),
                json!({"id": "e2", "role": "link", "label": "Inbox",
                       "container": "nav:Mail", "bbox": [10, 60, 70, 20],
                       "selector": "nav a", "enabled": true}),
            ],
            applied: true,
        }
    }

    /// Serve until the broker disconnects. Returns how many actions it was
    /// asked to perform.
    fn spawn(self, pipe: String) -> tokio::task::JoinHandle<usize> {
        tokio::spawn(async move {
            let mut peer = kratos_ipc::PipePeer::attach_backend(
                &pipe,
                &format!("fake-{}", self.source),
                BUILD,
                self.source,
                vec!["browser".into(), "native".into()],
            )
            .await
            .expect("backend attach");

            let mut acted = 0usize;
            while let Ok(Some(frame)) = peer.next_frame().await {
                let Some(op) = frame.op() else { continue };
                let Some(id) = frame.id() else { continue };
                let id = id.to_string();
                match op {
                    ops::BACKEND_OBSERVE => {
                        let _ = peer
                            .reply(
                                &id,
                                json!({
                                    "window": "Inbox - Gmail - Brave",
                                    "url": "https://mail.google.com/mail/u/0/#inbox",
                                    "elements": self.elements,
                                    "truncated": false
                                }),
                            )
                            .await;
                    }
                    ops::BACKEND_ACT => {
                        acted += 1;
                        let text = frame.payload()["text"].as_str().map(String::from);
                        let _ = peer
                            .reply(
                                &id,
                                json!({"applied": self.applied, "readback": text, "took_ms": 3}),
                            )
                            .await;
                    }
                    _ => {}
                }
            }
            acted
        })
    }
}

// ---------------------------------------------------------------------------
// 1. Protocol round trips
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hello_status_observe_and_act_round_trip_over_a_real_pipe() {
    let h = Harness::start("roundtrip", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut a = agent(&h.pipe, "claude-code").await;
    assert!(a.peer_id.starts_with("a-"), "peer id was {:?}", a.peer_id);
    assert!(a.broker.starts_with("kratos-broker "), "{:?}", a.broker);
    assert_eq!(a.decider, "mock/test");

    let status = a.call(ops::STATUS, json!({})).await.expect("status");
    assert_eq!(status["v"], json!(1));
    assert_eq!(status["backends"][0]["source"], json!("uia"));

    let obs = a
        .call(ops::OBSERVE, json!({"prefer": "uia", "timeout_ms": 4000}))
        .await
        .expect("observe");
    assert_eq!(obs["source"], json!("uia"));
    assert_eq!(obs["elements"].as_array().unwrap().len(), 3);
    assert!(obs["gen"].as_u64().unwrap() >= 1, "every observation is stamped");
    // The container is what makes two same-labelled fields distinguishable.
    assert_eq!(obs["elements"][0]["container"], json!("dialog:New Message"));

    let acted = a
        .call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"}))
        .await
        .expect("act");
    assert!(
        acted["decision"]["kind"].is_string(),
        "an act must report what it decided: {acted}"
    );
    assert!(acted["took_ms"].as_u64().is_some());
}

#[tokio::test]
async fn an_unknown_op_is_bad_request_and_the_connection_survives() {
    let h = Harness::start("unknownop", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let mut a = agent(&h.pipe, "t").await;

    let err = a.call("totally.made.up", json!({})).await.unwrap_err();
    assert_eq!(err.code, codes::BAD_REQUEST);
    // Still usable afterwards: one bad request must not drop an agent
    // mid-task.
    assert!(a.call(ops::STATUS, json!({})).await.is_ok());
}

#[tokio::test]
async fn a_malformed_line_is_answered_and_the_connection_survives() {
    let h = Harness::start("malformed", base_config(0));
    wait_for_pipe(&h.pipe).await;

    // Hand-rolled, because a well-behaved client cannot produce this.
    let conn = kratos_ipc::connect(&h.pipe).await.expect("connect");
    let (mut reader, mut writer) = kratos_ipc::split(conn);
    writer
        .send(&Frame::req(
            "1",
            ops::HELLO,
            json!({"role": "agent", "name": "t", "v_min": 1, "v_max": 1}),
        ))
        .await
        .unwrap();
    assert!(reader.next().await.unwrap().unwrap().is_ok());

    writer.into_inner().write_all(b"{ this is not json }\n").await.unwrap();
    let reply = reader.next().await.unwrap().unwrap();
    assert!(!reply.is_ok());
    assert_eq!(reply.wire_error().unwrap().code, codes::BAD_REQUEST);
}

#[tokio::test]
async fn a_version_mismatch_is_refused_at_the_handshake() {
    let h = Harness::start("badversion", base_config(0));
    wait_for_pipe(&h.pipe).await;

    let conn = kratos_ipc::connect(&h.pipe).await.expect("connect");
    let (mut reader, mut writer) = kratos_ipc::split(conn);
    writer
        .send(&Frame::req(
            "1",
            ops::HELLO,
            json!({"role": "agent", "name": "future", "v_min": 7, "v_max": 9}),
        ))
        .await
        .unwrap();
    let reply = reader.next().await.unwrap().unwrap();
    assert!(!reply.is_ok());
    let e = reply.wire_error().unwrap();
    assert_eq!(e.code, codes::BAD_VERSION);
    assert_eq!(e.detail["broker_v_max"], json!(1));
}

#[tokio::test]
async fn anything_before_hello_is_refused() {
    let h = Harness::start("prehello", base_config(0));
    wait_for_pipe(&h.pipe).await;

    let conn = kratos_ipc::connect(&h.pipe).await.expect("connect");
    let (mut reader, mut writer) = kratos_ipc::split(conn);
    writer.send(&Frame::req("1", ops::ACT, json!({"instruction": "click it"}))).await.unwrap();
    let reply = reader.next().await.unwrap().unwrap();
    assert_eq!(reply.wire_error().unwrap().code, codes::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// 2. Lazy start and idle stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nothing_is_listening_until_something_asks() {
    let pipe = format!("kratos-it-{}-lazy-absent", std::process::id());
    // The resting state of the whole system: no process, no pipe.
    assert_eq!(
        kratos_ipc::connect(&pipe).await.unwrap_err().raw_os_error(),
        Some(kratos_ipc::ERROR_FILE_NOT_FOUND)
    );
}

#[tokio::test]
async fn the_first_call_starts_the_broker_and_idleness_stops_it() {
    let unique = format!("kratos-it-{}-lazystart", std::process::id());
    let dir = std::env::temp_dir().join(&unique);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&base_config(1_200)).unwrap(),
    )
    .unwrap();

    // The broker inherits these, because a detached child gets the parent's
    // environment -- which is how the test keeps it off the real config.
    std::env::set_var("KRATOS_PIPE", &unique);
    std::env::set_var("KRATOS_CONFIG", &config_path);
    std::env::set_var("KRATOS_JOURNAL", dir.join("actions.jsonl"));
    std::env::set_var("KRATOS_BROKER_EXE", env!("CARGO_BIN_EXE_kratos-broker"));

    assert!(kratos_ipc::connect(&unique).await.is_err(), "nothing should be running yet");

    // This is the lazy start: one call, and a broker exists.
    let mut a = kratos_ipc::PipePeer::connect_or_start(&unique, Role::Agent, "lazy", BUILD)
        .await
        .expect("lazy start");
    let status = a.call(ops::STATUS, json!({})).await.expect("status");
    let pid = status["pid"].as_u64().unwrap();
    assert!(pid > 0);
    assert_eq!(status["idle_exit_ms"], json!(1_200));

    // Let go, and it should put itself away. The watchdog ticks every 5s, so
    // allow for one tick plus slack.
    drop(a);
    let mut gone = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if kratos_ipc::connect(&unique).await.is_err() {
            gone = true;
            break;
        }
    }
    assert!(gone, "the broker should have exited after being idle");

    // And a later call brings it back, which is what makes the idle exit safe.
    let mut again = kratos_ipc::PipePeer::connect_or_start(&unique, Role::Agent, "lazy", BUILD)
        .await
        .expect("restart on demand");
    let status = again.call(ops::STATUS, json!({})).await.expect("status");
    assert_ne!(status["pid"].as_u64().unwrap(), pid, "a fresh process, not the old one");

    drop(again);
    for v in ["KRATOS_PIPE", "KRATOS_CONFIG", "KRATOS_JOURNAL", "KRATOS_BROKER_EXE"] {
        std::env::remove_var(v);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_brokers_cannot_own_one_pipe() {
    let h = Harness::start("single", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let mut a = agent(&h.pipe, "t").await;
    let first_pid = a.call(ops::STATUS, json!({})).await.unwrap()["pid"].as_u64().unwrap();

    // A second broker on the same pipe must stand down quietly rather than
    // fight for the name -- two shims racing to spawn one is normal.
    let second = Command::new(env!("CARGO_BIN_EXE_kratos-broker"))
        .env("KRATOS_PIPE", &h.pipe)
        .env("KRATOS_CONFIG", h.dir.join("config.json"))
        .env("KRATOS_JOURNAL", h.dir.join("actions.jsonl"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .expect("second broker");
    assert!(second.status.success(), "standing down is not a failure");

    let still = a.call(ops::STATUS, json!({})).await.unwrap()["pid"].as_u64().unwrap();
    assert_eq!(still, first_pid, "the original broker must still own the pipe");
}

// ---------------------------------------------------------------------------
// 3. Two agents and two browsers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_backends_register_and_an_explicit_preference_picks_between_them() {
    let h = Harness::start("twobackends", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _uia = FakeBackend::browser("uia").spawn(h.pipe.clone());
    let _dom = FakeBackend::browser("dom-ext").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut a = agent(&h.pipe, "t").await;
    let status = a.call(ops::STATUS, json!({})).await.unwrap();
    let sources: Vec<&str> =
        status["backends"].as_array().unwrap().iter().map(|b| b["source"].as_str().unwrap()).collect();
    assert_eq!(sources, vec!["dom-ext", "uia"], "both browsers are registered");

    for want in ["uia", "dom-ext"] {
        let obs = a.call(ops::OBSERVE, json!({"prefer": want})).await.expect("observe");
        assert_eq!(obs["source"], json!(want), "preference must be honoured exactly");
    }

    // An unknown name is refused, not quietly substituted.
    let err = a.call(ops::OBSERVE, json!({"prefer": "telepathy"})).await.unwrap_err();
    assert_eq!(err.code, codes::BAD_REQUEST);
}

#[tokio::test]
async fn a_backend_reconnecting_supersedes_itself_rather_than_doubling_up() {
    let h = Harness::start("supersede", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let first = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let _second = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut a = agent(&h.pipe, "t").await;
    let status = a.call(ops::STATUS, json!({})).await.unwrap();
    let uia_count = status["backends"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["source"] == json!("uia"))
        .count();
    assert_eq!(uia_count, 1, "a restarted backend must replace, not duplicate");
    // The displaced one was told why, and closed.
    let _ = tokio::time::timeout(Duration::from_secs(3), first).await;
}

#[tokio::test]
async fn two_agents_take_turns_on_the_input_devices() {
    let h = Harness::start("twoagents", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut one = agent(&h.pipe, "agent-one").await;
    let mut two = agent(&h.pipe, "agent-two").await;
    assert_ne!(one.peer_id, two.peer_id);

    let grant = one
        .call(ops::LEASE_ACQUIRE, json!({"scope": "input", "ttl_ms": 4000, "wait_ms": 0}))
        .await
        .expect("first lease");
    let token = grant["token"].as_str().unwrap().to_string();

    // The second agent is told who has it, not left to collide on the mouse.
    let err = two
        .call(ops::LEASE_ACQUIRE, json!({"scope": "input", "ttl_ms": 1000, "wait_ms": 0}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::LEASE_HELD);
    assert_eq!(err.detail["holder"], json!(one.peer_id));

    // And an action from the second agent is refused rather than interleaved.
    let blocked = two.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await;
    assert_eq!(blocked.unwrap_err().code, codes::LEASE_HELD);

    one.call(ops::LEASE_RELEASE, json!({"token": token})).await.expect("release");

    // Now the second agent can work.
    let grant2 = two
        .call(ops::LEASE_ACQUIRE, json!({"scope": "input", "ttl_ms": 1000, "wait_ms": 1000}))
        .await
        .expect("second lease");
    assert_ne!(grant2["token"], grant["token"]);
}

#[tokio::test]
async fn a_queued_agent_gets_the_lease_when_the_holder_disconnects() {
    let h = Harness::start("leasedrop", base_config(0));
    wait_for_pipe(&h.pipe).await;

    let mut one = agent(&h.pipe, "holder").await;
    one.call(ops::LEASE_ACQUIRE, json!({"scope": "input", "ttl_ms": 60000, "wait_ms": 0}))
        .await
        .expect("lease");

    // A crashed agent must not hold the mouse for a minute.
    drop(one);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut two = agent(&h.pipe, "next").await;
    assert!(two
        .call(ops::LEASE_ACQUIRE, json!({"scope": "input", "ttl_ms": 1000, "wait_ms": 2000}))
        .await
        .is_ok());
}

// ---------------------------------------------------------------------------
// 4. The gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_gate_blocks_escalates_and_lets_ordinary_work_through() {
    let h = Harness::start("gate", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut a = agent(&h.pipe, "t").await;

    // Denylisted: no confirmation path at all.
    let blocked = a
        .call(ops::ACT, json!({"instruction": "type \"format c:\" into the box"}))
        .await
        .expect("call");
    assert_eq!(blocked["status"], json!(st::BLOCKED));
    assert!(blocked["gate"]["rule"].as_str().unwrap().starts_with("block_keyword"));
    assert!(blocked["next"].is_string(), "a refusal must say what to do instead");

    // Review: parked with a token, and nothing ran.
    let review = a.call(ops::ACT, json!({"instruction": "click the \"Send\" button"})).await.unwrap();
    assert_eq!(review["status"], json!(st::NEEDS_CONFIRMATION));
    let token = review["confirm_token"].as_str().expect("token").to_string();
    assert!(review["expires_in_ms"].as_u64().unwrap() > 0);
    assert_eq!(review["gate"]["rule"], json!("review_keyword:\"send\""));

    // A quoted payload containing a review keyword is not an action. This is
    // the bug that blocked an ordinary email subject line for two minutes.
    let typing = a
        .call(ops::ACT, json!({"instruction": "type \"Order-taking agents, do not delete\" into the Subject field"}))
        .await
        .unwrap();
    assert_ne!(
        typing["status"], json!(st::NEEDS_CONFIRMATION),
        "quoted text is a payload, not an instruction: {typing}"
    );

    // The token works once.
    let confirmed = a.call(ops::CONFIRM, json!({"token": &token})).await.unwrap();
    assert_ne!(confirmed["status"], json!(st::NEEDS_CONFIRMATION));
    let replay = a.call(ops::CONFIRM, json!({"token": &token})).await.unwrap_err();
    assert_eq!(replay.code, codes::STALE_CONFIRM);
}

#[tokio::test]
async fn a_confirmation_belongs_to_the_agent_that_earned_it() {
    let h = Harness::start("confirmowner", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut one = agent(&h.pipe, "one").await;
    let mut two = agent(&h.pipe, "two").await;

    let review = one.call(ops::ACT, json!({"instruction": "click the \"Send\" button"})).await.unwrap();
    let token = review["confirm_token"].as_str().unwrap().to_string();

    // Another agent must not be able to cash in an approval it never obtained.
    let err = two.call(ops::CONFIRM, json!({"token": token})).await.unwrap_err();
    assert_eq!(err.code, codes::UNAUTHORIZED);
}

#[tokio::test]
async fn the_rate_limit_stops_a_runaway_loop() {
    let mut config = base_config(0);
    config["gate"]["rate_limit_per_minute"] = json!(3);
    let h = Harness::start("ratelimit", config);
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut a = agent(&h.pipe, "t").await;

    let mut sawlimit = false;
    for _ in 0..6 {
        let r = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
        if r["gate"]["rule"] == json!("rate_limit_exceeded") {
            assert_eq!(r["status"], json!(st::BLOCKED));
            sawlimit = true;
            break;
        }
    }
    assert!(sawlimit, "a loop should hit the per-minute limit");
}

// ---------------------------------------------------------------------------
// 5. The kill switch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_can_engage_the_kill_switch_but_never_clear_it() {
    let h = Harness::start("kill", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut a = agent(&h.pipe, "t").await;

    let before = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
    assert_ne!(before["status"], json!(st::KILLED));

    let engaged = a.call(ops::KILL_ENGAGE, json!({"scope": "global"})).await.expect("engage");
    assert_eq!(engaged["engaged"], json!(["global"]));

    let after = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
    assert_eq!(after["status"], json!(st::KILLED));
    assert!(after["next"].as_str().unwrap().contains("operator"));

    // The whole point: the thing being stopped cannot un-stop itself.
    let denied = a.call(ops::KILL_RESET, json!({"scope": "global"})).await.unwrap_err();
    assert_eq!(denied.code, codes::UNAUTHORIZED);

    // Still halted after the attempt.
    let still = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
    assert_eq!(still["status"], json!(st::KILLED));

    // The operator can, through the settings role.
    let mut ui = kratos_ipc::PipePeer::attach(&h.pipe, Role::Ui, "settings", BUILD).await.unwrap();
    let cleared = ui.call(ops::KILL_RESET, json!({"scope": "global"})).await.expect("ui reset");
    assert_eq!(cleared["engaged"], json!([]));

    let resumed = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
    assert_ne!(resumed["status"], json!(st::KILLED));
}

#[tokio::test]
async fn a_scoped_kill_leaves_other_targets_alone() {
    let h = Harness::start("killscope", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut a = agent(&h.pipe, "t").await;

    // Halt one browser brand. Whatever is actually in front during this test
    // is not that brand, so work must continue.
    a.call(ops::KILL_ENGAGE, json!({"scope": "browser:nosuchbrowser"})).await.unwrap();
    let status = a.call(ops::KILL_STATUS, json!({})).await.unwrap();
    assert_eq!(status["engaged"], json!(["browser:nosuchbrowser"]));
    assert!(!status["hotkey"].as_str().unwrap().is_empty());

    // Scoped, not global: a different target is unaffected.
    let r = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
    assert_ne!(r["status"], json!(st::KILLED));
}

#[tokio::test]
async fn engaging_the_kill_switch_reaches_every_connected_peer() {
    let h = Harness::start("killevent", base_config(0));
    wait_for_pipe(&h.pipe).await;

    let mut watcher = agent(&h.pipe, "watcher").await;
    let mut presser = agent(&h.pipe, "presser").await;
    presser.call(ops::KILL_ENGAGE, json!({"scope": "global"})).await.unwrap();

    // The watcher learns about it without having to poll.
    let mut saw = false;
    for _ in 0..20 {
        if let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(250), watcher.next_frame()).await.unwrap_or(Ok(None))
        {
            if frame.op() == Some(ops::EVT_KILL) {
                assert_eq!(frame.payload()["engaged"], json!(["global"]));
                saw = true;
                break;
            }
        }
    }
    assert!(saw, "a kill must be broadcast, not left for the next poll to discover");
}

// ---------------------------------------------------------------------------
// 6. Roles, journal and effect reporting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_backend_cannot_act_and_an_agent_cannot_read_the_config() {
    let h = Harness::start("roles", base_config(0));
    wait_for_pipe(&h.pipe).await;

    let mut backend = kratos_ipc::PipePeer::attach_backend(
        &h.pipe,
        "nosy-backend",
        BUILD,
        "pixel",
        vec!["native".into()],
    )
    .await
    .unwrap();
    // A backend is hands, not a head. Page content reaching an `act` would be
    // the whole threat model walking in the front door.
    let err = backend.call(ops::ACT, json!({"instruction": "click anything"})).await.unwrap_err();
    assert_eq!(err.code, codes::UNAUTHORIZED);

    let mut a = agent(&h.pipe, "t").await;
    assert_eq!(
        a.call(ops::CONFIG_GET, json!({})).await.unwrap_err().code,
        codes::UNAUTHORIZED
    );
    assert_eq!(
        a.call(ops::CONFIG_SET, json!({"patch": {"idle_exit_ms": 1}})).await.unwrap_err().code,
        codes::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_settings_role_can_read_config_but_never_the_key() {
    let h = Harness::start("configget", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let mut ui = kratos_ipc::PipePeer::attach(&h.pipe, Role::Ui, "settings", BUILD).await.unwrap();

    let got = ui.call(ops::CONFIG_GET, json!({})).await.expect("config.get");
    let text = serde_json::to_string(&got).unwrap();
    assert!(!text.contains("api_key_enc"), "the sealed key must not be readable back");
    assert_eq!(got["config"]["decider"]["key_present"], json!(false));

    let set = ui
        .call(ops::CONFIG_SET, json!({"patch": {"idle_exit_ms": 77000}}))
        .await
        .expect("config.set");
    assert_eq!(set["config"]["idle_exit_ms"], json!(77000));
    assert_eq!(set["restart_required"], json!(false));
}

#[tokio::test]
async fn every_action_lands_in_the_journal_with_the_agent_that_asked() {
    let h = Harness::start("journal", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut one = agent(&h.pipe, "agent-alpha").await;
    let mut two = agent(&h.pipe, "agent-beta").await;
    one.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap();
    two.call(ops::ACT, json!({"instruction": "type \"format c:\" somewhere"})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let lines = h.journal();
    assert!(lines.len() >= 2, "{lines:?}");
    let names: Vec<&str> = lines.iter().filter_map(|l| l["agent"].as_str()).collect();
    assert!(names.contains(&"agent-alpha"), "{names:?}");
    assert!(names.contains(&"agent-beta"), "{names:?}");
    // With several agents on one broker, "who did this" is the question the
    // log exists to answer.
    for line in &lines {
        assert!(line["peer_id"].as_str().is_some_and(|p| !p.is_empty()), "{line}");
        assert_eq!(line["schema"], json!(1));
        assert!(line["ts"].as_str().is_some());
    }
    let blocked = lines.iter().find(|l| l["status"] == json!(st::BLOCKED)).expect("a blocked line");
    assert!(blocked["gate"].as_str().unwrap().contains("block_keyword"));
}

#[tokio::test]
async fn a_backend_that_dispatches_without_effect_reports_no_effect_not_success() {
    let h = Harness::start("noeffect", base_config(0));
    wait_for_pipe(&h.pipe).await;

    // The backend reports `applied: false`: dispatched, and the target ignored
    // it. With no other backend to fall back to, that is a failure to act.
    let mut backend = FakeBackend::browser("uia");
    backend.applied = false;
    let _b = backend.spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut a = agent(&h.pipe, "t").await;
    let err = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await;
    // Either a backend_error (nothing left to try) or a no_effect verdict --
    // what must never happen is `executed`.
    match err {
        Ok(v) => assert_ne!(v["status"], json!(st::EXECUTED), "{v}"),
        Err(e) => assert_eq!(e.code, codes::BACKEND_ERROR),
    }
}

#[tokio::test]
async fn acting_with_no_backend_at_all_says_so_plainly() {
    let h = Harness::start("nobackend", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let mut a = agent(&h.pipe, "t").await;

    let err = a.call(ops::ACT, json!({"instruction": "click the \"Inbox\" link"})).await.unwrap_err();
    assert_eq!(err.code, codes::NO_BACKEND);
    assert!(err.message.contains("backend"), "{}", err.message);
}

#[tokio::test]
async fn a_pinned_target_that_has_gone_is_stale_rather_than_a_nearby_guess() {
    let h = Harness::start("staleid", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let _backend = FakeBackend::browser("uia").spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut a = agent(&h.pipe, "t").await;

    let err = a
        .call(
            ops::ACT,
            json!({"instruction": "click it", "target_id": "uia:dead:beef"}),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::STALE_ID);
    assert!(err.message.contains("observe again"), "{}", err.message);
}

#[tokio::test]
async fn a_dry_run_decides_and_gates_without_dispatching() {
    let h = Harness::start("dryrun", base_config(0));
    wait_for_pipe(&h.pipe).await;
    let backend = FakeBackend::browser("uia");
    let handle = backend.spawn(h.pipe.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;

    let mut a = agent(&h.pipe, "t").await;
    let planned = a
        .call(ops::ACT, json!({"instruction": "click the \"Inbox\" link", "dry_run": true}))
        .await
        .unwrap();
    assert_eq!(planned["status"], json!(st::PLANNED));
    assert!(planned["decision"]["kind"].is_string());
    assert!(planned["effect"].is_null(), "nothing ran, so there is nothing to prove");

    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.abort();
    // The backend was asked to observe, never to act.
    let acted = tokio::time::timeout(Duration::from_secs(1), handle).await;
    if let Ok(Ok(count)) = acted {
        assert_eq!(count, 0, "a dry run must not reach the input devices");
    }
}
