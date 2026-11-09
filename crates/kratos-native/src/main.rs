//! kratos-native: perception and actuation in Rust.
//!
//! A backend peer like any other -- it speaks the same protocol as the Python
//! one and the browser extension, and the broker does not know or care which
//! of them answers. It exists because the Python path costs 223ms to read 24
//! elements and 2.1s to read 168, and almost all of that is round trips.
//!
//! Run it with no arguments and it attaches to the broker. Two other modes
//! exist for building and checking this thing, and they need no broker:
//!
//! ```text
//! kratos-native --bench [window...] [--out FILE]   time windows, repeatedly
//! kratos-native --dump  [window]                   write a fixture to stdout
//! ```

mod uia;

use std::time::{Duration, Instant};

use kratos_protocol::{codes, ops, WireError};
use serde_json::json;

const BUILD: &str = concat!("kratos-native ", env!("CARGO_PKG_VERSION"));
const SOURCE: &str = "uia";

/// Reconnect backoff, as the browser host uses.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("kratos-native: could not start the runtime: {e}");
            std::process::exit(1);
        }
    };

    match args.first().map(String::as_str) {
        Some("--bench") => bench(&args[1..]),
        Some("--dump") => dump(args.get(1).map(String::as_str)),
        _ => runtime.block_on(serve()),
    }
}

// ---------------------------------------------------------------------------
// As a backend
// ---------------------------------------------------------------------------

async fn serve() {
    let pipe = std::env::var("KRATOS_PIPE").unwrap_or_else(|_| "kratos.v1".to_string());
    let uia = match uia::Uia::start() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("kratos-native: {e}");
            std::process::exit(1);
        }
    };

    let mut backoff = BACKOFF_MIN;
    loop {
        let peer = match kratos_ipc::PipePeer::attach_backend(
            &pipe,
            "kratos-native",
            BUILD,
            SOURCE,
            vec!["native".to_string(), "browser".to_string()],
        )
        .await
        {
            Ok(p) => {
                backoff = BACKOFF_MIN;
                eprintln!("kratos-native: attached to the broker as {SOURCE}");
                p
            }
            Err(e) => {
                eprintln!("kratos-native: no broker yet ({e}); retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        pump(peer, &uia).await;
        eprintln!("kratos-native: lost the broker; waiting to reconnect");
    }
}

async fn pump(mut peer: kratos_ipc::PipePeer, uia: &uia::Uia) {
    loop {
        let frame = match peer.next_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) if e.is_fatal() => return,
            Err(_) => continue,
        };
        if frame.op() == Some(ops::EVT_SUPERSEDED) {
            eprintln!("kratos-native: superseded by another {SOURCE} backend; exiting");
            std::process::exit(0);
        }
        let (Some(op), Some(id)) = (frame.op(), frame.id()) else { continue };
        let (op, id) = (op.to_string(), id.to_string());
        let payload = frame.payload().clone();

        let reply = match op.as_str() {
            ops::BACKEND_OBSERVE => observe(uia, &payload),
            ops::BACKEND_ACT => Err(WireError::new(
                codes::BACKEND_ERROR,
                "kratos-native does not act yet; this is the UI Automation reading phase",
            )),
            _ => Err(WireError::new(codes::BAD_REQUEST, format!("unknown op {op}"))),
        };

        let sent = match reply {
            Ok(v) => peer.reply(&id, v).await,
            Err(e) => peer.reply_error(&id, e).await,
        };
        if sent.is_err() {
            return;
        }
    }
}

fn observe(uia: &uia::Uia, payload: &serde_json::Value) -> Result<serde_json::Value, WireError> {
    let req: kratos_protocol::BackendObserveReq = serde_json::from_value(payload.clone())
        .map_err(|e| WireError::new(codes::BAD_REQUEST, format!("unusable observe: {e}")))?;

    // The broker resolved the window already and says which one. Matching the
    // title again here is a second search that can disagree with the first --
    // two windows of one application share a title often enough -- so the
    // handle wins when there is one.
    let hwnd = match req.hwnd {
        Some(h) if h != 0 => h as isize,
        _ => match window::find(req.window.as_deref()) {
            Some(h) => h,
            None => {
                return Err(WireError::new(
                    codes::NO_TARGET,
                    match req.window.as_deref() {
                        Some(w) => format!("no visible window matches {w:?}"),
                        None => "there is no foreground window".to_string(),
                    },
                ))
            }
        },
    };

    let obs = uia
        .observe(hwnd)
        .map_err(|e| WireError::new(codes::BACKEND_ERROR, e))?;
    Ok(json!({
        "window": obs.window,
        "url": null,
        "elements": obs.elements,
        "truncated": obs.truncated,
        "took_ms": obs.fetch_ms + obs.walk_ms,
    }))
}

// ---------------------------------------------------------------------------
// Finding a window without the broker's help
// ---------------------------------------------------------------------------

mod window {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
    };

    pub fn find(needle: Option<&str>) -> Option<isize> {
        match needle.map(str::trim).filter(|n| !n.is_empty()) {
            None => {
                let h = unsafe { GetForegroundWindow() };
                (!h.is_invalid()).then_some(h.0 as isize)
            }
            Some(n) => by_title(n),
        }
    }

    pub fn title(hwnd: isize) -> String {
        let h = HWND(hwnd as *mut _);
        unsafe {
            let len = GetWindowTextLengthW(h);
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(h, &mut buf);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }

    struct Search {
        needle: String,
        found: Option<isize>,
    }

    pub fn by_title(needle: &str) -> Option<isize> {
        let mut search = Search { needle: needle.to_lowercase(), found: None };
        unsafe {
            let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
        }
        search.found
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        if IsWindowVisible(hwnd).as_bool() {
            let t = title(hwnd.0 as isize);
            if !t.is_empty() && t.to_lowercase().contains(&search.needle) {
                search.found = Some(hwnd.0 as isize);
                return false.into();
            }
        }
        true.into()
    }
}

// ---------------------------------------------------------------------------
// Measuring, and fixtures
// ---------------------------------------------------------------------------

/// Time windows repeatedly and print -- or write -- the distribution.
///
/// Repeatedly, because the first read of a window is not the interesting one:
/// a provider that has just been woken is doing work this design cannot
/// remove, and the number that matters is what an agent pays per step in a
/// session. Both are reported.
fn bench(args: &[String]) {
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from);
    let windows: Vec<&str> = args
        .iter()
        .take_while(|a| *a != "--out")
        .map(String::as_str)
        .collect();

    let uia = match uia::Uia::start() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("kratos-native: {e}");
            std::process::exit(1);
        }
    };

    let targets: Vec<Option<&str>> =
        if windows.is_empty() { vec![None] } else { windows.iter().map(|w| Some(*w)).collect() };

    let mut rows = Vec::new();
    for want in targets {
        let Some(hwnd) = window::find(want) else {
            eprintln!("no window matches {want:?}; skipping");
            continue;
        };
        match measure(&uia, hwnd, want) {
            Some(r) => {
                println!(
                    "{:<34} {:>4} el   cold {:>6.1}ms   p50 {:>6.1}ms   p90 {:>6.1}ms   \
                     fetch {:>3}ms",
                    r.window, r.elements, r.cold, r.p50, r.p90, r.fetch
                );
                rows.push(r);
            }
            None => eprintln!("could not read {want:?}"),
        }
    }

    if let Some(path) = out {
        let doc = report(&rows);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&path, doc) {
            Ok(()) => println!("\nwritten: {}", path.display()),
            Err(e) => eprintln!("could not write {}: {e}", path.display()),
        }
    }
}

struct Row {
    window: String,
    elements: usize,
    cold: f64,
    p50: f64,
    p90: f64,
    max: f64,
    fetch: u64,
}

const RUNS: usize = 21;

/// `label` is what the operator asked for, not what the window calls itself.
///
/// A live window title is the operator's business -- the file open in
/// Notepad, the folder in Explorer, the page in a browser -- and this table
/// is committed. The search string they typed says which application it was
/// without saying what was in it.
fn measure(uia: &uia::Uia, hwnd: isize, label: Option<&str>) -> Option<Row> {
    let mut cold = 0.0;
    let mut samples: Vec<f64> = Vec::new();
    let mut fetches: Vec<u64> = Vec::new();
    let mut elements = 0usize;
    let mut window = String::new();

    for i in 0..RUNS {
        let started = Instant::now();
        let o = uia.observe(hwnd).ok()?;
        let total = started.elapsed().as_micros() as f64 / 1000.0;
        if i == 0 {
            cold = total;
            window = match label {
                Some(l) => l.to_string(),
                None => "the window in front".to_string(),
            };
        } else {
            samples.push(total);
            fetches.push(o.fetch_ms);
        }
        elements = o.elements.len();
    }
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    fetches.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    Some(Row {
        window,
        elements,
        cold,
        p50: at(0.5),
        p90: at(0.9),
        max: at(1.0),
        fetch: fetches[fetches.len() / 2],
    })
}

/// The section that goes into `docs/benchmarks.md`.
///
/// Written separately and inlined there, because these are **live** numbers
/// -- they depend on which windows are open and what is in them -- and
/// `kratos-bench` is the offline, reproducible half. Mixing the two in one
/// generated file would make the reproducible one a lie.
fn report(rows: &[Row]) -> String {
    let mut s = String::new();
    s.push_str("<!-- generated: kratos-native --bench ... --out docs/native-benchmarks.md -->\n");
    s.push_str("| window | elements | cold ms | p50 ms | p90 ms | max ms | of which fetch |\n");
    s.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {:.1} | **{:.1}** | {:.1} | {:.1} | {}ms |\n",
            r.window.replace('|', "/"),
            r.elements,
            r.cold,
            r.p50,
            r.p90,
            r.max,
            r.fetch
        ));
    }
    s.push_str(&format!(
        "\nOne cached fetch per window, {RUNS} runs each, the first reported separately as \
         cold. `fetch` is the single cross-process call; everything else is reading the cache \
         locally, which is why it is the whole cost.\n"
    ));
    s
}

/// Write one window's elements as JSON, for a fixture.
fn dump(window: Option<&str>) {
    let uia = match uia::Uia::start() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("kratos-native: {e}");
            std::process::exit(1);
        }
    };
    let Some(hwnd) = window::find(window) else {
        eprintln!("no window matches {window:?}");
        std::process::exit(1);
    };
    match uia.observe(hwnd) {
        Ok(o) => println!("{}", serde_json::to_string_pretty(&o).unwrap_or_default()),
        Err(e) => {
            eprintln!("observe failed: {e}");
            std::process::exit(1);
        }
    }
}
