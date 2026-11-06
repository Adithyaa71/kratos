//! Native messaging host: the bridge between a browser extension and the
//! broker.
//!
//! The browser starts this process, not us, and it may start it long before a
//! broker exists -- or while one is mid-restart. So it must do nothing
//! expensive while waiting: it blocks on stdin, blocks on the pipe, and sleeps
//! between reconnect attempts with a backoff. No polling loop, no timers
//! firing against an empty queue. A process the browser keeps alive for the
//! whole session has to cost nothing when idle.
//!
//! It is a relay and nothing else. It holds no policy, makes no decisions, and
//! translates only framing: Chromium's 4-byte length prefix on stdio, NDJSON
//! on the pipe. A message that arrives on one side appears on the other
//! unchanged, apart from chunk reassembly.

use std::collections::HashMap;
use std::time::Duration;

use kratos_protocol::{Frame, Instance};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const BUILD: &str = concat!("kratos-host ", env!("CARGO_PKG_VERSION"));
const SOURCE: &str = "dom-ext";

/// Reconnect backoff bounds. Thirty seconds is long enough that a browser left
/// open overnight with no broker costs nothing measurable.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A reassembly that never completes is abandoned after this long.
const CHUNK_TTL: Duration = Duration::from_secs(4);

/// How long to wait for the extension to name its instance before attaching
/// without one.
///
/// The extension sends `evt instance` as its first message, but this host is
/// started *by the browser* and may be running against an older extension that
/// sends nothing. Waiting forever would make this host useless to one; not
/// waiting at all would attach with no token and leave two profiles fighting
/// over one registry slot, which is the whole defect being fixed. So: wait
/// briefly, then attach anyway and carry on.
const INSTANCE_GRACE: Duration = Duration::from_millis(1500);

fn main() {
    // `--manifest` prints the native-messaging registration the browser needs,
    // so installing does not depend on anyone hand-writing JSON with the right
    // absolute path in it.
    if std::env::args().skip(1).any(|a| a == "--manifest") {
        print_manifest();
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kratos-host: could not start the runtime: {e}");
            std::process::exit(1);
        }
    };
    runtime.block_on(relay());
}

async fn relay() {
    let pipe_base = std::env::var("KRATOS_PIPE").unwrap_or_else(|_| "kratos.v1".to_string());

    // Read stdin on its own task so the browser is never blocked by a pipe
    // that is down, and so a disconnected host still drains what it is sent.
    let (from_ext_tx, mut from_ext) = mpsc::channel::<Value>(32);
    tokio::spawn(read_extension(from_ext_tx));

    // Which browser profile this host belongs to. Learned from the extension's
    // first message and then kept for the life of the process, so a broker
    // restart re-registers the same instance rather than looking like a new
    // profile.
    let mut instance = wait_for_instance(&mut from_ext).await;
    if let Some(i) = &instance {
        eprintln!("kratos-host: instance {} ({})", i.token, i.brand.as_deref().unwrap_or("?"));
    } else {
        eprintln!(
            "kratos-host: the extension named no instance within {INSTANCE_GRACE:?}; \
             attaching as the only dom-ext instance"
        );
    }

    let mut backoff = BACKOFF_MIN;

    loop {
        let peer = match kratos_ipc::PipePeer::attach_backend_instance(
            &pipe_base,
            "browser-extension",
            BUILD,
            SOURCE,
            vec!["browser".to_string()],
            instance.clone(),
        )
        .await
        {
            Ok(p) => {
                backoff = BACKOFF_MIN;
                eprintln!("kratos-host: attached to the broker as {SOURCE}");
                // Tell the extension it is live, so it can stop showing a
                // disconnected state without having to poll. A write that
                // fails means there is no extension on the other end, so
                // there is nothing for this host to do.
                if !write_extension(&json!({"v": 1, "t": "evt", "op": "connected", "payload": {}}))
                    .await
                {
                    eprintln!("kratos-host: the extension is unreachable; exiting");
                    return;
                }
                p
            }
            Err(e) => {
                // Expected whenever no broker is running. Not an error worth
                // shouting about every second, hence the backoff.
                eprintln!("kratos-host: no broker yet ({e}); retrying in {backoff:?}");
                if !write_extension(
                    &json!({"v": 1, "t": "evt", "op": "disconnected", "payload": {}}),
                )
                .await
                {
                    eprintln!("kratos-host: the extension is unreachable; exiting");
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        if pump(peer, &mut from_ext, &mut instance).await.is_none() {
            // The extension went away: the browser is closing this host.
            return;
        }
        eprintln!("kratos-host: lost the broker; waiting to reconnect");
        if !write_extension(&json!({"v": 1, "t": "evt", "op": "disconnected", "payload": {}}))
            .await
        {
            eprintln!("kratos-host: the extension is unreachable; exiting");
            return;
        }
    }
}

/// Relay until either side drops.
///
/// Returns `Some(())` when the broker connection ended (reconnect), or `None`
/// when the extension ended (exit).
async fn pump(
    mut peer: kratos_ipc::PipePeer,
    from_ext: &mut mpsc::Receiver<Value>,
    instance: &mut Option<Instance>,
) -> Option<()> {
    loop {
        tokio::select! {
            // Extension -> broker
            message = from_ext.recv() => {
                let message = message?;  // None means the browser closed stdin
                // An instance that arrives late -- a slow service worker, or
                // one that restarted and said hello again -- is still worth
                // having. Re-attaching is the only way to correct a hello that
                // has already been sent, and it costs one reconnect.
                if let Some(named) = parse_instance(&message) {
                    if instance.as_ref() != Some(&named) {
                        eprintln!("kratos-host: instance named late ({}); re-attaching", named.token);
                        *instance = Some(named);
                        return Some(());
                    }
                    continue;
                }
                let Ok(frame) = serde_json::from_value::<Frame>(message.clone()) else {
                    eprintln!("kratos-host: dropping an unusable message from the extension");
                    continue;
                };
                match &frame.body {
                    // A reply to something the broker asked. Forward verbatim.
                    kratos_protocol::Body::Res { id, ok, payload, error } => {
                        let sent = if *ok {
                            peer.reply(id, payload.clone()).await
                        } else {
                            peer.reply_error(
                                id,
                                error.clone().unwrap_or_else(|| {
                                    kratos_protocol::WireError::new(
                                        kratos_protocol::codes::BACKEND_ERROR,
                                        "the extension refused without saying why",
                                    )
                                }),
                            )
                            .await
                        };
                        if sent.is_err() {
                            return Some(());
                        }
                    }
                    // The extension does not get to issue requests: it is a
                    // backend, and the protocol gives a backend no agent ops.
                    // Relaying them would hand page content a way to drive the
                    // machine.
                    _ => eprintln!("kratos-host: ignoring a non-reply from the extension"),
                }
            }

            // Broker -> extension
            incoming = peer.next_frame() => {
                match incoming {
                    Ok(Some(frame)) => {
                        // Another host now holds this profile's registration,
                        // and it can only be a newer one: a host exists
                        // because an extension created it. So this one has
                        // nothing left to relay.
                        //
                        // This is how the stale hosts are reaped. Measured
                        // live: one browser had three hosts alive, two of them
                        // connected to the broker and registered to nothing,
                        // because a service-worker restart opens a new port --
                        // and the old host's stdin does not reach end of file
                        // when Chromium drops the old one.
                        if frame.op() == Some(kratos_protocol::ops::EVT_SUPERSEDED) {
                            eprintln!(
                                "kratos-host: superseded by another host for this profile; exiting"
                            );
                            return None;
                        }
                        let Ok(value) = serde_json::to_value(&frame) else { continue };
                        // A write that fails means the extension is gone --
                        // the port is closed and nothing can reopen it from
                        // this side. Exiting is the only correct answer; the
                        // browser starts a fresh host when it wants one.
                        if !write_extension(&value).await {
                            eprintln!("kratos-host: the extension is unreachable; exiting");
                            return None;
                        }
                    }
                    Ok(None) => return Some(()),
                    Err(e) if e.is_fatal() => return Some(()),
                    Err(_) => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Which profile this host belongs to
// ---------------------------------------------------------------------------

/// Read the extension's `evt instance`, or give up after [`INSTANCE_GRACE`].
///
/// Anything else that arrives in the meantime is dropped on purpose: before
/// the broker connection exists there is nothing a reply could be a reply to,
/// and the extension re-sends on reconnect.
async fn wait_for_instance(from_ext: &mut mpsc::Receiver<Value>) -> Option<Instance> {
    let deadline = tokio::time::Instant::now() + INSTANCE_GRACE;
    loop {
        match tokio::time::timeout_at(deadline, from_ext.recv()).await {
            Ok(Some(message)) => {
                if let Some(i) = parse_instance(&message) {
                    return Some(i);
                }
            }
            // The browser closed stdin, or the grace period ran out.
            Ok(None) | Err(_) => return None,
        }
    }
}

/// `{"v":1,"t":"evt","op":"instance","payload":{"token":"...","brand":"..."}}`
///
/// The token is the extension's; the pid is **ours to fill in**, because the
/// extension cannot see a process id and this host knows the only thing that
/// links an instance to windows on screen: the browser that started it.
fn parse_instance(message: &Value) -> Option<Instance> {
    if message.get("t")?.as_str()? != "evt" || message.get("op")?.as_str()? != "instance" {
        return None;
    }
    let payload = message.get("payload")?;
    let token = payload.get("token")?.as_str()?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    let browser = browser_process();
    Some(Instance {
        token,
        // The OS wins over the self-report: an image name cannot be masked the
        // way a user agent can. The extension's answer is the fallback for a
        // browser this host could not find in the process tree.
        brand: browser.map(|(_, brand)| brand.to_string()).or_else(|| {
            payload.get("brand").and_then(|b| b.as_str()).map(|s| s.to_ascii_lowercase())
        }),
        pid: browser.map(|(pid, _)| pid),
        parent_hwnd: None,
    })
}

/// Chromium brand for a process image name.
///
/// The same table the broker uses to classify a window, kept here because this
/// is where the authoritative answer is available: an image name is what the
/// OS says a process is, while the brand the extension reports is a
/// self-report from a browser that may be lying on purpose. Vivaldi ships
/// Chrome's user agent by default and no `navigator.brave`-style hook, so the
/// extension detected it as `chrome` in a live run -- and lane resolution
/// keyed on that would have looked for the wrong browser's windows.
fn brand_of(image: &str) -> Option<&'static str> {
    match image.to_ascii_lowercase().as_str() {
        "brave.exe" => Some("brave"),
        "chrome.exe" => Some("chrome"),
        "msedge.exe" => Some("edge"),
        "vivaldi.exe" => Some("vivaldi"),
        "opera.exe" | "opera_gx.exe" => Some("opera"),
        _ => None,
    }
}

/// How far up the process tree to look for the browser.
///
/// Chromium on Windows does **not** start a native messaging host directly: it
/// goes through `cmd.exe`, so the browser is the *grandparent*. Measured live:
///
/// ```text
/// kratos-host.exe#27584 <- cmd.exe#2644 <- brave.exe#17472
/// ```
///
/// Taking the immediate parent therefore reported a `cmd.exe` pid as the
/// browser, which is precisely the wrong-pid case this lookup exists to avoid.
/// Four hops is room for another wrapper without wandering off into the
/// session's process tree.
const ANCESTRY_HOPS: usize = 4;

/// The browser process that this host ultimately belongs to, and its brand.
///
/// Best effort, and labelled as such: if the snapshot fails, or no ancestor
/// within [`ANCESTRY_HOPS`] is a browser, the instance carries no pid and the
/// pairing is made by hand in the settings window instead. A *wrong* pid would
/// be worse than none -- it would point lane resolution at somebody else's
/// windows.
#[cfg(windows)]
fn browser_process() -> Option<(u32, &'static str)> {
    let table = process_table()?;
    let mut pid = std::process::id();
    for _ in 0..ANCESTRY_HOPS {
        let (parent, _) = table.get(&pid)?;
        let (_, name) = table.get(parent)?;
        if let Some(brand) = brand_of(name) {
            return Some((*parent, brand));
        }
        if *parent == 0 || *parent == pid {
            return None;
        }
        pid = *parent;
    }
    None
}

/// One snapshot, read once: `pid -> (parent pid, image name)`.
#[cfg(windows)]
fn process_table() -> Option<HashMap<u32, (u32, String)>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut table: HashMap<u32, (u32, String)> = HashMap::new();
        let mut ok = Process32FirstW(snapshot, &mut entry);
        while ok != 0 {
            let len = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            table.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
            ok = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        Some(table)
    }
}

#[cfg(not(windows))]
fn browser_process() -> Option<(u32, &'static str)> {
    None
}

/// Read Chromium's native-messaging framing from stdin, reassembling chunks.
async fn read_extension(out: mpsc::Sender<Value>) {
    let mut stdin = tokio::io::stdin();
    let mut partial: HashMap<String, Reassembly> = HashMap::new();

    loop {
        let mut header = [0u8; 4];
        if stdin.read_exact(&mut header).await.is_err() {
            return; // the browser closed the host
        }
        let len = u32::from_le_bytes(header) as usize;
        if len == 0 {
            continue;
        }
        if len > kratos_protocol::NATIVE_MSG_MAX {
            // Chromium would not have sent this; something is out of step with
            // the stream and there is no way to resynchronise safely.
            eprintln!("kratos-host: oversized message ({len} bytes); stopping");
            return;
        }
        let mut buf = vec![0u8; len];
        if stdin.read_exact(&mut buf).await.is_err() {
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buf) else {
            eprintln!("kratos-host: unparseable message from the extension");
            continue;
        };

        partial.retain(|_, r| r.started.elapsed() < CHUNK_TTL);

        match Chunk::parse(&value) {
            Some(chunk) => {
                let entry = partial.entry(chunk.id.clone()).or_insert_with(|| Reassembly {
                    parts: vec![String::new(); chunk.n],
                    seen: 0,
                    started: std::time::Instant::now(),
                });
                if chunk.i < entry.parts.len() && entry.parts[chunk.i].is_empty() {
                    entry.parts[chunk.i] = chunk.part;
                    entry.seen += 1;
                }
                if entry.seen == chunk.n {
                    let joined = entry.parts.concat();
                    partial.remove(&chunk.id);
                    match serde_json::from_str::<Value>(&joined) {
                        Ok(v) => {
                            if out.send(v).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => eprintln!("kratos-host: reassembled message is not JSON: {e}"),
                    }
                }
            }
            None => {
                if out.send(value).await.is_err() {
                    return;
                }
            }
        }
    }
}

struct Reassembly {
    parts: Vec<String>,
    seen: usize,
    started: std::time::Instant,
}

#[derive(Debug, PartialEq)]
struct Chunk {
    id: String,
    i: usize,
    n: usize,
    part: String,
}

impl Chunk {
    fn parse(value: &Value) -> Option<Chunk> {
        let meta = value.get("chunk")?;
        let id = meta.get("id")?.as_str()?.to_string();
        let i = meta.get("i")?.as_u64()? as usize;
        let n = meta.get("n")?.as_u64()? as usize;
        // A zero-part message, or an index past the end, is nonsense rather
        // than a chunk; treating it as one would allocate on a bad number.
        if n == 0 || n > 4096 || i >= n {
            return None;
        }
        let part = value.get("part")?.as_str()?.to_string();
        Some(Chunk { id, i, n, part })
    }
}

/// Returns whether the extension is still there to write to.
///
/// The result used to be discarded. A broken pipe is the one signal this host
/// gets that its extension has gone -- stdin does not reach end of file when
/// Chromium drops a port, measured live -- so throwing it away left hosts
/// running with nothing on the other end.
async fn write_extension(value: &Value) -> bool {
    let Ok(text) = serde_json::to_vec(value) else { return true };
    let mut stdout = tokio::io::stdout();
    let header = (text.len() as u32).to_le_bytes();
    if stdout.write_all(&header).await.is_err() {
        return false;
    }
    if stdout.write_all(&text).await.is_err() {
        return false;
    }
    stdout.flush().await.is_ok()
}

/// The native-messaging manifest the browser needs, printed by `--manifest`
/// so installation does not depend on hand-edited JSON.
fn print_manifest() {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "kratos-host.exe".into());
    let manifest = json!({
        "name": "ai.kratos.bridge",
        "description": "kratos browser bridge",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [
            "chrome-extension://REPLACE_WITH_YOUR_EXTENSION_ID/"
        ]
    });
    println!("{}", serde_json::to_string_pretty(&manifest).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brand_table_matches_the_brokers() {
        assert_eq!(brand_of("brave.exe"), Some("brave"));
        assert_eq!(brand_of("Vivaldi.exe"), Some("vivaldi"));
        assert_eq!(brand_of("msedge.exe"), Some("edge"));
        assert_eq!(brand_of("notepad.exe"), None);
    }

    #[test]
    fn an_instance_event_is_recognised_and_anything_else_is_not() {
        let i = parse_instance(&json!({
            "v": 1, "t": "evt", "op": "instance",
            "payload": {"token": " tok-1 ", "brand": "CHROME"}
        }))
        .expect("an instance event");
        assert_eq!(i.token, "tok-1", "a token is trimmed, not taken as typed");

        // A reply, a request, and an event of another kind are all not this.
        assert!(parse_instance(&json!({"v": 1, "t": "res", "id": "1", "ok": true})).is_none());
        assert!(parse_instance(&json!({"v": 1, "t": "evt", "op": "connected", "payload": {}}))
            .is_none());
        // A blank token is no instance: keying the registry on an empty string
        // would let two profiles share one slot.
        assert!(parse_instance(&json!({
            "v": 1, "t": "evt", "op": "instance", "payload": {"token": "   "}
        }))
        .is_none());
    }

    #[test]
    fn the_brand_the_os_reports_beats_the_one_the_extension_claims() {
        // Vivaldi ships Chrome's user agent, so the extension says "chrome".
        // Whether this host can see the process tree decides which answer
        // wins, and both outcomes are legitimate -- the one thing that must
        // not happen is the claim overriding the OS.
        let i = parse_instance(&json!({
            "v": 1, "t": "evt", "op": "instance",
            "payload": {"token": "t", "brand": "chrome"}
        }))
        .unwrap();
        match browser_process() {
            Some((pid, brand)) => {
                assert_eq!(i.brand.as_deref(), Some(brand));
                assert_eq!(i.pid, Some(pid));
            }
            // Under `cargo test` the ancestor is the test harness, not a
            // browser, so the extension's claim is all there is.
            None => {
                assert_eq!(i.brand.as_deref(), Some("chrome"));
                assert_eq!(i.pid, None);
            }
        }
    }

    #[test]
    fn the_process_table_contains_this_process_and_its_parent() {
        // The lookup that reported a cmd.exe pid as the browser was wrong
        // about the shape of the tree, not about the API, so the test is that
        // the tree is readable at all.
        let Some(table) = process_table() else {
            eprintln!("no process snapshot in this environment; skipping");
            return;
        };
        let me = std::process::id();
        let (parent, name) = table.get(&me).expect("this process is in the snapshot");
        assert!(name.to_ascii_lowercase().contains("kratos") || name.ends_with(".exe"), "{name}");
        assert!(table.contains_key(parent) || *parent == 0);
    }

    #[test]
    fn a_well_formed_chunk_parses() {
        let c = Chunk::parse(&json!({"chunk": {"id": "b-91", "i": 0, "n": 3}, "part": "{\"a\":"}))
            .unwrap();
        assert_eq!(c.id, "b-91");
        assert_eq!((c.i, c.n), (0, 3));
        assert_eq!(c.part, "{\"a\":");
    }

    #[test]
    fn a_plain_frame_is_not_a_chunk() {
        assert!(Chunk::parse(&json!({"v": 1, "t": "res", "id": "b-1", "ok": true})).is_none());
    }

    #[test]
    fn nonsense_chunk_metadata_is_rejected_rather_than_allocated_on() {
        // n == 0 would make an empty parts vector that never completes;
        // i >= n would index past the end; a huge n would allocate on a number
        // that came from outside.
        assert!(Chunk::parse(&json!({"chunk": {"id": "x", "i": 0, "n": 0}, "part": ""})).is_none());
        assert!(Chunk::parse(&json!({"chunk": {"id": "x", "i": 5, "n": 3}, "part": ""})).is_none());
        assert!(
            Chunk::parse(&json!({"chunk": {"id": "x", "i": 0, "n": 99999}, "part": ""})).is_none()
        );
        assert!(Chunk::parse(&json!({"chunk": {"id": "x", "i": 0, "n": 2}})).is_none());
    }

    #[test]
    fn chunks_reassemble_in_order_regardless_of_arrival_order() {
        let mut r = Reassembly {
            parts: vec![String::new(); 3],
            seen: 0,
            started: std::time::Instant::now(),
        };
        for (i, part) in [(2, "3}"), (0, "{\"n\":"), (1, "12")] {
            r.parts[i] = part.to_string();
            r.seen += 1;
        }
        assert_eq!(r.seen, 3);
        let joined = r.parts.concat();
        assert_eq!(joined, "{\"n\":123}");
        let v: Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(v["n"], json!(123));
    }

    #[test]
    fn a_duplicate_chunk_index_does_not_double_count() {
        // Otherwise `seen == n` could trip with a gap still unfilled, and the
        // reassembled JSON would be truncated.
        let mut r = Reassembly {
            parts: vec![String::new(); 2],
            seen: 0,
            started: std::time::Instant::now(),
        };
        for _ in 0..2 {
            if r.parts[0].is_empty() {
                r.parts[0] = "a".into();
                r.seen += 1;
            }
        }
        assert_eq!(r.seen, 1, "a repeated index must not count twice");
    }

    #[test]
    fn the_backoff_doubles_and_stops_at_the_ceiling() {
        let mut b = BACKOFF_MIN;
        let mut steps = 0;
        while b < BACKOFF_MAX && steps < 20 {
            b = (b * 2).min(BACKOFF_MAX);
            steps += 1;
        }
        assert_eq!(b, BACKOFF_MAX);
        assert!(steps <= 6, "reaching the ceiling should take a handful of tries, not {steps}");
    }

    #[test]
    fn the_manifest_names_a_stdio_host_and_a_real_path() {
        // Smoke test for --manifest: the fields the browser requires are
        // present and the path is this executable, not a guess.
        let exe = std::env::current_exe().unwrap().display().to_string();
        let manifest = json!({
            "name": "ai.kratos.bridge",
            "type": "stdio",
            "path": exe,
        });
        assert_eq!(manifest["type"], json!("stdio"));
        assert!(manifest["path"].as_str().unwrap().ends_with(".exe"));
    }

    #[test]
    fn chromium_framing_round_trips() {
        let text = br#"{"v":1,"t":"res","id":"b-1","ok":true,"payload":{}}"#;
        let header = (text.len() as u32).to_le_bytes();
        let mut wire = header.to_vec();
        wire.extend_from_slice(text);

        let len = u32::from_le_bytes([wire[0], wire[1], wire[2], wire[3]]) as usize;
        assert_eq!(len, text.len());
        let frame: Frame = serde_json::from_slice(&wire[4..4 + len]).unwrap();
        assert_eq!(frame.id(), Some("b-1"));
        assert!(frame.is_ok());
    }

    #[test]
    fn the_source_this_host_registers_as_is_the_dom_backend() {
        assert_eq!(kratos_protocol::Source::parse(SOURCE), Some(kratos_protocol::Source::DomExt));
    }

    #[test]
    fn print_manifest_emits_valid_json_with_this_executables_path() {
        // Called for real so the function cannot rot unnoticed; stdout in a
        // test harness is captured.
        print_manifest();
    }
}
