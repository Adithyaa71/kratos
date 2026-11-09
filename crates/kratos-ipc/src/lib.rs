//! Transport for kratos: a Windows named pipe carrying NDJSON frames.
//!
//! No TCP anywhere. Nothing here opens a socket, binds a port, or resolves a
//! host; the only channel is a named pipe whose DACL admits one SID. That is a
//! deliberate limit, not an omission -- a tool that can drive the mouse and
//! read the screen should not be reachable over a network interface at all,
//! loopback included, because loopback is reachable by every other process on
//! the machine.

pub mod peer;
pub mod win;

pub use peer::{Peer, PipePeer};

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use kratos_protocol::{Frame, MAX_LINE};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};

/// `\\.\pipe\<base>.<user-sid>`.
pub fn pipe_path(base: &str) -> io::Result<String> {
    Ok(format!(r"\\.\pipe\{}.{}", base, win::current_user_sid()?))
}

/// The single-instance mutex name that pairs with a pipe name.
pub fn instance_name(base: &str) -> io::Result<String> {
    Ok(format!("{}.{}", base, win::current_user_sid()?))
}

/// The name of the machine-wide input guard for this Windows user.
///
/// Per user, not per pipe: one person has one mouse, so a second broker on a
/// second pipe name must not be able to hand out a second input lease. The
/// override exists because the test suite deliberately runs several brokers at
/// once and they must not contend for the real one -- it is a test seam, like
/// `KRATOS_PIPE`, and is not a way to get a second lease on a real machine
/// (nothing but a test sets it).
pub fn input_guard_name() -> io::Result<String> {
    if let Some(explicit) = std::env::var_os("KRATOS_INPUT_GUARD") {
        let s = explicit.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Ok(format!("kratos.input.{s}"));
        }
    }
    Ok(format!("kratos.input.{}", win::current_user_sid()?))
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RecvError {
    /// The peer sent a line this build cannot parse. Recoverable: the frame is
    /// dropped and the connection stays up, because one bad line from one
    /// agent should not disconnect it mid-task.
    Parse(serde_json::Error),
    /// Over 8 MiB without a newline. Not recoverable: the sender is not
    /// speaking this protocol, and continuing would mean buffering without
    /// bound.
    TooLarge,
    Io(io::Error),
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvError::Parse(e) => write!(f, "malformed frame: {e}"),
            RecvError::TooLarge => write!(f, "frame exceeds {MAX_LINE} bytes"),
            RecvError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RecvError {}

impl RecvError {
    /// Whether the connection can carry on after this.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, RecvError::Parse(_))
    }
}

pub struct FrameReader<R> {
    inner: BufReader<R>,
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(r: R) -> Self {
        FrameReader { inner: BufReader::with_capacity(64 * 1024, r), line: Vec::with_capacity(4096) }
    }

    /// The next frame, or `None` at end of stream.
    ///
    /// Reads through `fill_buf` rather than `read_until` so the 8 MiB ceiling
    /// is enforced *while* reading. `read_until` would happily allocate past
    /// the limit first and let us complain afterwards, which is the wrong
    /// order for a limit whose purpose is to bound memory.
    pub async fn next(&mut self) -> Result<Option<Frame>, RecvError> {
        self.line.clear();
        loop {
            let available = self.inner.fill_buf().await.map_err(RecvError::Io)?;
            if available.is_empty() {
                if self.line.is_empty() {
                    return Ok(None);
                }
                // A final line with no trailing newline is still a frame.
                break;
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    self.line.extend_from_slice(&available[..i]);
                    self.inner.consume(i + 1);
                    break;
                }
                None => {
                    let n = available.len();
                    self.line.extend_from_slice(available);
                    self.inner.consume(n);
                    if self.line.len() > MAX_LINE {
                        return Err(RecvError::TooLarge);
                    }
                }
            }
        }
        // Tolerate CRLF and blank keep-alive lines.
        while self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        if self.line.is_empty() {
            // Recurse-free: ask again rather than returning a phantom frame.
            return Box::pin(self.next()).await;
        }
        serde_json::from_slice(&self.line).map(Some).map_err(RecvError::Parse)
    }
}

pub struct FrameWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    pub fn new(w: W) -> Self {
        FrameWriter { inner: w }
    }

    pub async fn send(&mut self, frame: &Frame) -> io::Result<()> {
        let mut buf = serde_json::to_vec(frame)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        buf.push(b'\n');
        // One write for the whole line: a pipe peer reading line-wise must
        // never see a frame split across two writes with a flush between.
        self.inner.write_all(&buf).await?;
        self.inner.flush().await
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// Split a duplex stream into a reader and a writer half.
pub fn split<S: AsyncRead + AsyncWrite + Unpin>(
    s: S,
) -> (FrameReader<tokio::io::ReadHalf<S>>, FrameWriter<tokio::io::WriteHalf<S>>) {
    let (r, w) = tokio::io::split(s);
    (FrameReader::new(r), FrameWriter::new(w))
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Listener that hands out one `NamedPipeServer` per connection.
///
/// The first instance is created with `first_pipe_instance`, so a second
/// broker that got past the mutex still cannot bind the name: two independent
/// guards for the thing that must never happen twice.
pub struct PipeListener {
    path: String,
    security: win::OwnerOnlySecurity,
    pending: Option<NamedPipeServer>,
}

impl PipeListener {
    pub fn bind(base: &str) -> io::Result<Self> {
        let path = pipe_path(base)?;
        let security = win::OwnerOnlySecurity::for_current_user()?;
        let pending = Some(create_instance(&path, &security, true)?);
        Ok(PipeListener { path, security, pending })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Wait for the next client.
    ///
    /// The replacement instance is created *before* this returns, so there is
    /// no window in which the pipe name exists but has no listener -- a client
    /// connecting in that window would get `ERROR_FILE_NOT_FOUND` and
    /// needlessly try to spawn a second broker.
    pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        let server = match self.pending.take() {
            Some(s) => s,
            None => create_instance(&self.path, &self.security, false)?,
        };
        server.connect().await?;
        self.pending = Some(create_instance(&self.path, &self.security, false)?);
        Ok(server)
    }
}

fn create_instance(
    path: &str,
    security: &win::OwnerOnlySecurity,
    first: bool,
) -> io::Result<NamedPipeServer> {
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .in_buffer_size(64 * 1024)
            .out_buffer_size(64 * 1024)
            .create_with_security_attributes_raw(path, security.as_ptr())
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

const ERROR_PIPE_BUSY: i32 = 231;
pub const ERROR_FILE_NOT_FOUND: i32 = 2;

/// Connect once. `ERROR_FILE_NOT_FOUND` means no broker is listening, which
/// the caller may answer by starting one.
pub async fn connect(base: &str) -> io::Result<NamedPipeClient> {
    let path = pipe_path(base)?;
    connect_path(&path).await
}

pub async fn connect_path(path: &str) -> io::Result<NamedPipeClient> {
    // All instances busy is a transient condition -- the listener creates the
    // next one as soon as it accepts -- so it is worth a short retry rather
    // than reporting "no broker".
    for _ in 0..40 {
        match ClientOptions::new().open(path) {
            Ok(c) => return Ok(c),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::WouldBlock, "all pipe instances busy"))
}

/// Connect, starting the broker if it is not running (section 5.5).
///
/// The broker is never autostarted at logon; this is the only thing that
/// brings it up, on the first tool call that needs it.
pub async fn connect_or_start(base: &str, broker_exe: &std::path::Path) -> io::Result<NamedPipeClient> {
    let path = pipe_path(base)?;

    // Two full attempts: the pipe can legitimately vanish between the probe
    // and the open when two shims race and the losing broker exits.
    let mut last: io::Error = io::Error::other("unreachable");
    for attempt in 0..2 {
        match connect_path(&path).await {
            Ok(c) => return Ok(c),
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                if attempt == 0 {
                    spawn_detached(broker_exe)?;
                }
                // Backoff 25ms doubling to 400ms, ~3s total budget.
                let mut delay = 25u64;
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3_000);
                while std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    match connect_path(&path).await {
                        Ok(c) => return Ok(c),
                        Err(e2) => last = e2,
                    }
                    delay = (delay * 2).min(400);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        format!("broker did not come up at {path}: {last}"),
    ))
}

/// Start a process that outlives us and owns no console.
///
/// `DETACHED_PROCESS` is what makes the broker survive the agent exiting, and
/// is also why no console window flashes: a detached console application
/// inherits no console and allocates none. Nothing is inherited -- not stdio,
/// not handles -- so the broker cannot accidentally hold the agent's pipes
/// open after it quits.
pub fn spawn_detached(exe: &std::path::Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    std::process::Command::new(exe)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Where `kratos-broker.exe` is expected to live: beside the calling binary.
pub fn broker_exe() -> io::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("KRATOS_BROKER_EXE") {
        return Ok(std::path::PathBuf::from(p));
    }
    let me = std::env::current_exe()?;
    let dir = me
        .parent()
        .ok_or_else(|| io::Error::other("current exe has no parent directory"))?;
    Ok(dir.join("kratos-broker.exe"))
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A correlation id unique within this process.
///
/// Scoped per connection by the protocol, so a process-wide counter is more
/// than enough and avoids pulling in a UUID dependency for something nobody
/// reads.
pub fn next_id(prefix: &str) -> String {
    format!("{}-{}", prefix, COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kratos_protocol::{ops, Body};
    use serde_json::json;

    async fn roundtrip(lines: &str) -> Vec<Result<Option<Frame>, RecvError>> {
        let mut r = FrameReader::new(std::io::Cursor::new(lines.as_bytes().to_vec()));
        let mut out = vec![];
        loop {
            let got = r.next().await;
            let stop = matches!(got, Ok(None)) || matches!(&got, Err(e) if e.is_fatal());
            out.push(got);
            if stop {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn reads_a_sequence_of_frames() {
        let lines = concat!(
            r#"{"v":1,"t":"req","id":"1","op":"status","payload":{}}"#,
            "\n",
            r#"{"v":1,"t":"req","id":"2","op":"observe","payload":{}}"#,
            "\n"
        );
        let got = roundtrip(lines).await;
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].as_ref().unwrap().as_ref().unwrap().op(), Some("status"));
        assert_eq!(got[1].as_ref().unwrap().as_ref().unwrap().op(), Some("observe"));
        assert!(matches!(got[2], Ok(None)));
    }

    #[tokio::test]
    async fn tolerates_crlf_and_blank_lines() {
        let lines = "\r\n\r\n{\"v\":1,\"t\":\"req\",\"id\":\"1\",\"op\":\"status\"}\r\n";
        let got = roundtrip(lines).await;
        assert_eq!(got[0].as_ref().unwrap().as_ref().unwrap().id(), Some("1"));
    }

    #[tokio::test]
    async fn a_final_line_without_a_newline_still_parses() {
        let got = roundtrip(r#"{"v":1,"t":"req","id":"9","op":"status"}"#).await;
        assert_eq!(got[0].as_ref().unwrap().as_ref().unwrap().id(), Some("9"));
    }

    #[tokio::test]
    async fn a_bad_line_is_recoverable_and_does_not_drop_the_next_one() {
        let lines = concat!(
            "{not json at all}\n",
            r#"{"v":1,"t":"req","id":"2","op":"status","payload":{}}"#,
            "\n"
        );
        let mut r = FrameReader::new(std::io::Cursor::new(lines.as_bytes().to_vec()));
        let first = r.next().await;
        assert!(matches!(first, Err(RecvError::Parse(_))));
        assert!(!first.as_ref().unwrap_err().is_fatal());
        // The point: the connection carries on and the following frame is intact.
        let second = r.next().await.unwrap().unwrap();
        assert_eq!(second.id(), Some("2"));
    }

    #[tokio::test]
    async fn an_oversized_line_is_fatal_and_bounded() {
        let mut huge = Vec::with_capacity(MAX_LINE + 4096);
        huge.extend_from_slice(b"{\"v\":1,\"t\":\"req\",\"id\":\"1\",\"op\":\"");
        huge.resize(MAX_LINE + 2048, b'x');
        let mut r = FrameReader::new(std::io::Cursor::new(huge));
        let got = r.next().await;
        assert!(matches!(got, Err(RecvError::TooLarge)));
        assert!(got.unwrap_err().is_fatal());
    }

    #[tokio::test]
    async fn writer_emits_exactly_one_newline_terminated_line() {
        let mut buf: Vec<u8> = vec![];
        {
            let mut w = FrameWriter::new(&mut buf);
            w.send(&Frame::req("1", ops::STATUS, json!({}))).await.unwrap();
            w.send(&Frame::ok("1", json!({"up": true}))).await.unwrap();
        }
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
        // Each frame occupies exactly one line and nothing is split across
        // two, which is what a line-oriented reader depends on.
        for line in text.lines() {
            assert!(!line.is_empty());
            serde_json::from_str::<Frame>(line).expect(line);
        }
    }

    #[tokio::test]
    async fn frames_survive_the_wire_over_a_real_named_pipe() {
        let base = format!("kratos-test-pipe-{}", std::process::id());
        let mut listener = PipeListener::bind(&base).expect("bind");
        let path = listener.path().to_string();
        assert!(path.starts_with(r"\\.\pipe\kratos-test-pipe-"), "{path}");

        let server = tokio::spawn(async move {
            let conn = listener.accept().await.expect("accept");
            let (mut r, mut w) = split(conn);
            let got = r.next().await.unwrap().unwrap();
            assert_eq!(got.op(), Some("act"));
            let instruction = got.payload()["instruction"].as_str().unwrap().to_string();
            w.send(&Frame::ok(got.id().unwrap(), json!({"echo": instruction}))).await.unwrap();
        });

        let client = connect(&base).await.expect("connect");
        let (mut r, mut w) = split(client);
        w.send(&Frame::req("x1", ops::ACT, json!({"instruction": "click \"Send\""})))
            .await
            .unwrap();
        let res = r.next().await.unwrap().unwrap();
        assert!(res.is_ok());
        assert_eq!(res.payload()["echo"], json!("click \"Send\""));
        assert!(matches!(res.body, Body::Res { .. }));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_second_listener_cannot_steal_the_name() {
        let base = format!("kratos-test-dup-{}", std::process::id());
        let _first = PipeListener::bind(&base).expect("first binds");
        // first_pipe_instance makes this the second guard behind the mutex.
        assert!(PipeListener::bind(&base).is_err(), "a second broker must not bind");
    }

    #[tokio::test]
    async fn connecting_with_no_listener_reports_file_not_found() {
        let err = connect("kratos-test-absent-xyzzy").await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(ERROR_FILE_NOT_FOUND));
    }

    #[test]
    fn pipe_path_is_namespaced_by_sid() {
        let p = pipe_path("kratos.v1").unwrap();
        assert!(p.starts_with(r"\\.\pipe\kratos.v1.S-1-"), "{p}");
    }

    #[test]
    fn ids_are_unique() {
        let a = next_id("a");
        let b = next_id("a");
        assert_ne!(a, b);
        assert!(a.starts_with("a-"));
    }
}
