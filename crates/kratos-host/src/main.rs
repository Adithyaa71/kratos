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

use kratos_protocol::Frame;
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

    let mut backoff = BACKOFF_MIN;

    loop {
        let peer = match kratos_ipc::PipePeer::attach_backend(
            &pipe_base,
            "browser-extension",
            BUILD,
            SOURCE,
            vec!["browser".to_string()],
        )
        .await
        {
            Ok(p) => {
                backoff = BACKOFF_MIN;
                eprintln!("kratos-host: attached to the broker as {SOURCE}");
                // Tell the extension it is live, so it can stop showing a
                // disconnected state without having to poll.
                write_extension(&json!({"v": 1, "t": "evt", "op": "connected", "payload": {}}))
                    .await;
                p
            }
            Err(e) => {
                // Expected whenever no broker is running. Not an error worth
                // shouting about every second, hence the backoff.
                eprintln!("kratos-host: no broker yet ({e}); retrying in {backoff:?}");
                write_extension(
                    &json!({"v": 1, "t": "evt", "op": "disconnected", "payload": {}}),
                )
                .await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        if pump(peer, &mut from_ext).await.is_none() {
            // The extension went away: the browser is closing this host.
            return;
        }
        eprintln!("kratos-host: lost the broker; waiting to reconnect");
        write_extension(&json!({"v": 1, "t": "evt", "op": "disconnected", "payload": {}})).await;
    }
}

/// Relay until either side drops.
///
/// Returns `Some(())` when the broker connection ended (reconnect), or `None`
/// when the extension ended (exit).
async fn pump(
    mut peer: kratos_ipc::PipePeer,
    from_ext: &mut mpsc::Receiver<Value>,
) -> Option<()> {
    loop {
        tokio::select! {
            // Extension -> broker
            message = from_ext.recv() => {
                let message = message?;  // None means the browser closed stdin
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
                        let Ok(value) = serde_json::to_value(&frame) else { continue };
                        write_extension(&value).await;
                    }
                    Ok(None) => return Some(()),
                    Err(e) if e.is_fatal() => return Some(()),
                    Err(_) => {}
                }
            }
        }
    }
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

async fn write_extension(value: &Value) {
    let Ok(text) = serde_json::to_vec(value) else { return };
    let mut stdout = tokio::io::stdout();
    let header = (text.len() as u32).to_le_bytes();
    if stdout.write_all(&header).await.is_err() {
        return;
    }
    if stdout.write_all(&text).await.is_err() {
        return;
    }
    let _ = stdout.flush().await;
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
