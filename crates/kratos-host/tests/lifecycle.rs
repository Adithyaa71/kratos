//! The host's lifecycle, driven as a real process.
//!
//! The thing this host does that nothing else can test is survive being a
//! relay with a browser on one side and a broker on the other, both of which
//! may vanish. So the test runs the real `kratos-host.exe`, speaks Chromium's
//! native-messaging framing to its stdio the way a browser would, and serves
//! the pipe by hand the way the broker would.
//!
//! No broker binary and no browser: this crate cannot reference either, and
//! the point is the host's own behaviour.

use std::process::Stdio;
use std::time::Duration;

use kratos_protocol::{ops, Frame};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Long enough for a process to notice something and act on it, short enough
/// that a test that is going to fail does not take a minute to say so.
const GRACE: Duration = Duration::from_secs(5);

struct Browser {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// Kept so a failing test can say what the host thought it was doing: its
    /// stderr is invisible when a browser starts it, and that invisibility is
    /// most of why the stale-host bug took a live run plus a test to find.
    stderr: Option<tokio::process::ChildStderr>,
}

impl Browser {
    async fn stderr_so_far(&mut self) -> String {
        let Some(mut e) = self.stderr.take() else { return String::new() };
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(300), e.read_to_end(&mut buf)).await;
        String::from_utf8_lossy(&buf).to_string()
    }
}

impl Browser {
    /// Start the host as a browser would: pipes on stdio, `KRATOS_PIPE` in the
    /// environment, nothing else.
    fn start(pipe: &str) -> Browser {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kratos-host"))
            .env("KRATOS_PIPE", pipe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn kratos-host");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        Browser { child, stdin, stdout, stderr: Some(stderr) }
    }

    /// Chromium's framing: 4-byte little-endian length, then the JSON.
    async fn send(&mut self, value: &Value) {
        let text = serde_json::to_vec(value).unwrap();
        self.stdin.write_all(&(text.len() as u32).to_le_bytes()).await.expect("write header");
        self.stdin.write_all(&text).await.expect("write body");
        self.stdin.flush().await.expect("flush");
    }

    /// The next message the host writes, or `None` if it closed stdout.
    async fn recv(&mut self) -> Option<Value> {
        let mut header = [0u8; 4];
        self.stdout.read_exact(&mut header).await.ok()?;
        let len = u32::from_le_bytes(header) as usize;
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf).await.ok()?;
        serde_json::from_slice(&buf).ok()
    }

    async fn exited_within(&mut self, how_long: Duration) -> bool {
        tokio::time::timeout(how_long, self.child.wait()).await.is_ok()
    }
}

fn pipe_name(label: &str) -> String {
    format!("kratos-host-it-{}-{label}", std::process::id())
}

/// Stand in for the broker: own the pipe, complete one handshake, hand back
/// the connection so the test can send what it likes.
struct FakeBroker {
    listener: kratos_ipc::PipeListener,
}

impl FakeBroker {
    fn bind(pipe: &str) -> FakeBroker {
        FakeBroker { listener: kratos_ipc::PipeListener::bind(pipe).expect("bind pipe") }
    }

    /// Accept one connection, read its `hello`, answer it, and return the
    /// hello payload plus the live halves.
    async fn accept(
        &mut self,
    ) -> (
        Value,
        kratos_ipc::FrameReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>>,
        kratos_ipc::FrameWriter<tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>>,
    ) {
        let conn = self.listener.accept().await.expect("accept");
        let (mut reader, mut writer) = kratos_ipc::split(conn);
        let frame = reader.next().await.expect("read hello").expect("a hello");
        assert_eq!(frame.op(), Some(ops::HELLO), "the first message must be hello");
        let hello = frame.payload().clone();
        let id = frame.id().unwrap_or("1").to_string();
        writer
            .send(&Frame::ok(
                &id,
                json!({"v": 1, "peer_id": "b-1", "broker": "fake", "decider": "mock",
                       "kill_engaged": [], "backends": []}),
            ))
            .await
            .expect("send ack");
        (hello, reader, writer)
    }
}

// ---------------------------------------------------------------------------

/// P1: the extension names its profile, and the host passes it on.
#[tokio::test]
async fn the_hello_carries_the_instance_the_extension_named() {
    let pipe = pipe_name("instance");
    let mut broker = FakeBroker::bind(&pipe);
    let mut browser = Browser::start(&pipe);

    browser
        .send(&json!({"v": 1, "t": "evt", "op": "instance",
                      "payload": {"token": "tok-abc", "brand": "chrome"}}))
        .await;

    let (hello, _r, _w) = tokio::time::timeout(GRACE, broker.accept())
        .await
        .expect("the host must attach");

    assert_eq!(hello["source"], json!("dom-ext"));
    assert_eq!(hello["instance"]["token"], json!("tok-abc"));
    // The brand is the OS's answer where there is one, and the extension's
    // claim otherwise -- under `cargo test` the ancestor is the test harness,
    // so the claim stands.
    assert!(hello["instance"]["brand"].is_string(), "{hello}");

    // And the host tells the extension it is live.
    let evt = tokio::time::timeout(GRACE, browser.recv()).await.expect("a message").expect("json");
    assert_eq!(evt["op"], json!("connected"));
}

/// An older extension that says nothing still gets a working host, after the
/// grace period rather than never.
#[tokio::test]
async fn a_silent_extension_still_gets_a_host_without_an_instance() {
    let pipe = pipe_name("silent");
    let mut broker = FakeBroker::bind(&pipe);
    let _browser = Browser::start(&pipe);

    let (hello, _r, _w) = tokio::time::timeout(Duration::from_secs(8), broker.accept())
        .await
        .expect("the host must attach even with no instance named");
    assert_eq!(hello["source"], json!("dom-ext"));
    assert!(hello.get("instance").is_none() || hello["instance"].is_null(), "{hello}");
}

/// P1, the half the live run disproved: a host that is superseded must exit.
///
/// Measured on two real browsers, five hosts attached and three were
/// superseded -- and all five were still running eleven minutes later. A
/// service-worker restart opens a new port, Chromium does not close the old
/// host's stdin, so being superseded is the only notice the old host gets, and
/// it has to act on it.
#[tokio::test]
async fn a_superseded_host_exits() {
    let pipe = pipe_name("superseded");
    let mut broker = FakeBroker::bind(&pipe);
    let mut browser = Browser::start(&pipe);

    browser
        .send(&json!({"v": 1, "t": "evt", "op": "instance",
                      "payload": {"token": "tok-same", "brand": "brave"}}))
        .await;
    let (_hello, _r, mut writer) =
        tokio::time::timeout(GRACE, broker.accept()).await.expect("attach");

    // The broker's own supersede notice, verbatim.
    writer
        .send(&Frame::evt(
            ops::EVT_SUPERSEDED,
            json!({"reason": "this instance (tok-same) reconnected", "source": "dom-ext"}),
        ))
        .await
        .expect("send superseded");

    let exited = browser.exited_within(GRACE).await;
    assert!(
        exited,
        "a superseded host must exit rather than sit there registered to nothing.          host stderr:
{}",
        browser.stderr_so_far().await
    );
}

/// The browser closing is the ordinary end of a host's life.
#[tokio::test]
async fn closing_the_extension_port_ends_the_host() {
    let pipe = pipe_name("portclosed");
    let mut broker = FakeBroker::bind(&pipe);
    let mut browser = Browser::start(&pipe);
    browser
        .send(&json!({"v": 1, "t": "evt", "op": "instance", "payload": {"token": "tok-x"}}))
        .await;
    let (_h, _r, _w) = tokio::time::timeout(GRACE, broker.accept()).await.expect("attach");

    // What Chromium does when the port goes: stdin reaches end of file.
    drop(browser.stdin);
    browser.stdin = {
        // Replaced with a closed handle so the struct stays whole; nothing
        // writes to it after this.
        let mut dummy = Command::new("cmd")
            .args(["/c", "exit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dummy");
        dummy.stdin.take().expect("dummy stdin")
    };

    assert!(
        browser.exited_within(GRACE).await,
        "a host whose extension has gone has nothing to relay"
    );
}
