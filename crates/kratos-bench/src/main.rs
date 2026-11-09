//! What this costs, measured rather than claimed.
//!
//! Every number in the README comes from here, and anyone who clones the
//! repository can reproduce it on their own machine with one command. That is
//! the whole point: a latency figure you cannot re-run is marketing.
//!
//! Scope: the layers kratos itself owns -- the MCP shim, the pipe, the broker's
//! own handling, the gate, the candidate compression, the screen
//! fingerprints. Everything here runs offline against the mock decision model,
//! so the numbers are the framework's own overhead and nothing else.
//!
//! Deliberately *not* measured here:
//!
//! * the decision model's round trip, which is a network call to somebody
//!   else's GPU and tells you about their day, not this code;
//! * the perception backends, which are a separate process in a separate
//!   language -- `perception` prints its own timings and the README quotes
//!   them as what they are, observations from a live desktop rather than a
//!   benchmark.
//!
//! Usage:
//!   kratos-bench                 print a table
//!   kratos-bench --out FILE      write docs/benchmarks.md
//!   kratos-bench --json          one JSON object per line

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kratos_decide::Question;
use kratos_protocol::{ops, Element, Role};

/// One measured thing.
struct Row {
    name: &'static str,
    note: String,
    samples: Vec<f64>,
}

impl Row {
    fn new(name: &'static str, note: impl Into<String>, samples: Vec<f64>) -> Row {
        let mut samples = samples;
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Row { name, note: note.into(), samples }
    }

    /// Nearest-rank percentile. No interpolation: with a few hundred samples
    /// an interpolated p95 invents a number nothing actually measured.
    fn pct(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return f64::NAN;
        }
        let rank = ((p / 100.0) * self.samples.len() as f64).ceil() as usize;
        self.samples[rank.saturating_sub(1).min(self.samples.len() - 1)]
    }

    fn p50(&self) -> f64 {
        self.pct(50.0)
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn fmt(v: f64) -> String {
    if v.is_nan() {
        "--".to_string()
    } else if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else if v >= 1.0 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// A broker of our own
// ---------------------------------------------------------------------------

/// An isolated broker: its own pipe, config and journal, the mock decider, and
/// no key anywhere near it.
struct Rig {
    pipe: String,
    dir: PathBuf,
    config: PathBuf,
    child: Option<Child>,
}

impl Rig {
    fn prepare(label: &str) -> Rig {
        let pipe = format!("kratos-bench-{}-{label}", std::process::id());
        let dir = std::env::temp_dir().join(&pipe);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.json");
        let text = serde_json::to_string_pretty(&serde_json::json!({
            "idle_exit_ms": 0,
            "decider": {"provider": "mock", "model": "bench"},
            "kill": {"hotkey": "ctrl+alt+shift+f24", "hotkey_scope": "global"},
            "gate": {"rate_limit_per_minute": 100000, "confirm_ttl_ms": 60000},
            // The benchmark is not a new client asking for permission: an
            // unapproved harness may call status and whoami and nothing else,
            // and the approval is a dialog no benchmark can click.
            "harnesses": [
                {"id": "bench", "approved": true},
                {"id": "bench-hs", "approved": true},
                {"id": "mcp-client", "approved": true},
            ],
            // No browser lanes: this measures the broker's own overhead, and
            // a lane would make these numbers depend on which browser the
            // machine has open.
            "defaults": {"browser": "none"},
            "thresholds": {"default": 0.0}
        }))
        .unwrap();
        std::fs::write(&config, text).expect("config");
        Rig { pipe, dir, config, child: None }
    }

    fn spawn(&mut self) {
        let exe = exe_dir().join("kratos-broker.exe");
        let child = Command::new(exe)
            .env("KRATOS_PIPE", &self.pipe)
            .env("KRATOS_CONFIG", &self.config)
            .env("KRATOS_JOURNAL", self.dir.join("actions.jsonl"))
            .env_remove("KRATOS_API_KEY")
            .env_remove("TYPESAFE_API_KEY")
            .env_remove("JEV_API_KEY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kratos-broker.exe -- cargo build --release first");
        self.child = Some(child);
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawn, then hammer the pipe until a `status` comes back. This is what an
/// agent's first tool call actually waits for.
async fn cold_start(label: &str) -> Option<f64> {
    let mut rig = Rig::prepare(label);
    let t0 = Instant::now();
    rig.spawn();
    for _ in 0..600 {
        if let Ok(mut peer) =
            kratos_ipc::PipePeer::attach(&rig.pipe, Role::Agent, "bench", "bench").await
        {
            if peer.call(ops::STATUS, serde_json::json!({})).await.is_ok() {
                return Some(ms(t0.elapsed()));
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    None
}

// ---------------------------------------------------------------------------
// The MCP shim
// ---------------------------------------------------------------------------

/// `tools/list` is the call every harness makes at startup, often several
/// times a session, and it must not wake a broker to answer it.
fn mcp_tools_list(iters: usize) -> (Vec<f64>, Vec<f64>, usize, bool) {
    let exe = exe_dir().join("kratos-mcp.exe");
    let pipe = format!("kratos-bench-mcp-{}", std::process::id());
    let mut samples = Vec::new();
    let mut whole = Vec::new();
    let mut tools = 0usize;

    for _ in 0..iters {
        // What a harness actually waits for: the process does not exist yet.
        let spawned = Instant::now();
        let mut child = Command::new(&exe)
            .env("KRATOS_PIPE", &pipe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kratos-mcp.exe");

        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);

        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "kratos-bench", "version": "0"}}
        });
        writeln!(stdin, "{init}").unwrap();
        stdin.flush().unwrap();
        let _ = read_line(&mut reader);

        let t0 = Instant::now();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .unwrap();
        stdin.flush().unwrap();
        let line = read_line(&mut reader);
        samples.push(ms(t0.elapsed()));
        whole.push(ms(spawned.elapsed()));

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(list) = v["result"]["tools"].as_array() {
                tools = list.len();
            }
        }
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
    }

    // If listing the tools had started a broker, this pipe would now answer.
    let started = std::path::Path::new(&format!(r"\\.\pipe\{pipe}")).exists();
    (samples, whole, tools, started)
}

fn read_line(r: &mut impl std::io::BufRead) -> String {
    let mut s = String::new();
    let _ = r.read_line(&mut s);
    s
}

// ---------------------------------------------------------------------------
// In-process layers
// ---------------------------------------------------------------------------

/// A page-sized observation: what a real browser tab yields.
fn synthetic_elements(n: usize) -> Vec<Element> {
    (0..n)
        .map(|i| Element {
            id: format!("brave:w1237019988:t1237020121:f0:e{i}"),
            role: ["button", "link", "textbox", "combobox", "checkbox"][i % 5].to_string(),
            label: format!("Control number {i} on this page"),
            value: None,
            enabled: true,
            bbox: Some([10 + (i as i32 % 8) * 120, 60 + (i as i32 / 8) * 40, 110, 28]),
            container: Some(format!("form:section-{}", i / 8)),
            selector: Some(format!("#section-{} > *:nth-child({})", i / 8, i % 8 + 1)),
            sensitive: false,
            // A DOM observation is one source, so an element from it carries
            // no per-element source of its own.
            source: None,
            confidence: None,
        })
        .collect()
}

fn bench_compress(elements: &[Element], iters: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let (candidates, _) =
            kratos_decide::compress("click the Search button in the toolbar", elements, 255);
        out.push(ms(t0.elapsed()));
        std::hint::black_box(candidates);
    }
    out
}

fn bench_decide(elements: &[Element], iters: usize) -> Vec<f64> {
    let cfg = kratos_protocol::config::Decider {
        provider: "mock".into(),
        model: "bench".into(),
        ..Default::default()
    };
    let provider = kratos_decide::build(&cfg, String::new());
    let (candidates, _) =
        kratos_decide::compress("type \"hello\" into the Search box", elements, 255);

    let mut questions = BTreeMap::new();
    questions.insert(
        "kind".to_string(),
        Question::Choice {
            instructions: "Which kind of action does the operator's sentence ask for?".into(),
            criteria: kratos_protocol::Kind::ALL
                .iter()
                .map(|k| (k.as_str().to_string(), format!("the operator asks to {}", k.as_str())))
                .collect(),
        },
    );
    questions.insert(
        "target".to_string(),
        Question::Choice {
            instructions: "Which element is the operator pointing at?".into(),
            criteria: candidates
                .iter()
                .enumerate()
                .map(|(i, c)| ((i + 1).to_string(), c.criterion.clone()))
                .collect(),
        },
    );

    let state = serde_json::json!({
        "instruction": "type \"hello\" into the Search box",
        "window": "Search - Brave",
        "url": "https://example.com/search",
    });

    let mut out = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let answers = provider.ask(&state, &questions);
        out.push(ms(t0.elapsed()));
        std::hint::black_box(answers.is_ok());
    }
    out
}

fn bench_gate(iters: usize) -> Vec<f64> {
    let config = kratos_protocol::config::GateConfig {
        block_keywords: vec!["format c:".into(), "rm -rf".into(), "shutdown /s".into()],
        review_keywords: vec![
            "send".into(),
            "delete".into(),
            "pay".into(),
            "transfer".into(),
            "publish".into(),
        ],
        review_apps: vec!["outlook".into(), "banking".into()],
        review_domains: vec!["mail.google.com".into(), "github.com".into()],
        deny_domains: vec!["pastebin.com".into()],
        allowed_browsers: vec!["brave".into()],
        rate_limit_per_minute: 1_000_000,
        ..Default::default()
    };
    let gate = kratos_gate::SafetyGate::new(config);
    let target = kratos_gate::TargetRef {
        window_title: "Inbox (3) - someone@example.com - Brave".into(),
        url: Some("https://mail.google.com/mail/u/0/#inbox".into()),
        exe: Some("brave.exe".into()),
        brand: Some("brave".into()),
    };

    let mut out = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = Instant::now();
        let v = gate.classify("click the Send button in the compose window", &target);
        out.push(ms(t0.elapsed()));
        std::hint::black_box(v.category);
    }
    out
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    let mut rows: Vec<Row> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // --- the MCP shim ------------------------------------------------------
    let (samples, whole, tools, woke_broker) = mcp_tools_list(9);
    rows.push(Row::new(
        "mcp tools/list",
        format!("{tools} tools, answered from a static table"),
        samples,
    ));
    rows.push(Row::new(
        "mcp cold, spawn included",
        "process start + initialize + tools/list",
        whole,
    ));
    if woke_broker {
        notes.push(
            "WARNING: listing the tools started a broker. It is supposed to answer from a \
             static table."
                .into(),
        );
    }

    // --- cold start --------------------------------------------------------
    let mut cold = Vec::new();
    for i in 0..5 {
        if let Some(v) = cold_start(&format!("cold{i}")).await {
            cold.push(v);
        }
    }
    rows.push(Row::new("broker cold start", "spawn to first status reply", cold));

    // --- warm round trips --------------------------------------------------
    let mut rig = Rig::prepare("warm");
    rig.spawn();
    let mut peer = None;
    for _ in 0..600 {
        if let Ok(p) = kratos_ipc::PipePeer::attach(&rig.pipe, Role::Agent, "bench", "bench").await
        {
            peer = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let mut peer = peer.expect("broker never came up");

    let mut status = Vec::with_capacity(400);
    for _ in 0..400 {
        let t0 = Instant::now();
        let r = peer.call(ops::STATUS, serde_json::json!({})).await;
        status.push(ms(t0.elapsed()));
        assert!(r.is_ok(), "status failed: {r:?}");
    }
    rows.push(Row::new("status round trip", "agent -> pipe -> broker -> agent", status));

    let mut tail = Vec::with_capacity(100);
    for _ in 0..100 {
        let t0 = Instant::now();
        let r = peer.call(ops::LOG_TAIL, serde_json::json!({"n": 50})).await;
        tail.push(ms(t0.elapsed()));
        assert!(r.is_ok());
    }
    rows.push(Row::new("log.tail 50", "journal read over the pipe", tail));

    let mut handshake = Vec::with_capacity(50);
    for _ in 0..50 {
        let t0 = Instant::now();
        let p = kratos_ipc::PipePeer::attach(&rig.pipe, Role::Agent, "bench-hs", "bench").await;
        handshake.push(ms(t0.elapsed()));
        assert!(p.is_ok());
    }
    rows.push(Row::new("connect + handshake", "new client, ACL check, hello/ack", handshake));
    drop(peer);
    rig.kill();

    // --- the layers between a sentence and a click -------------------------
    let elements = synthetic_elements(64);
    rows.push(Row::new("candidate compression", "64 elements -> options", bench_compress(&elements, 200)));
    rows.push(Row::new("decide (mock, offline)", "two typed questions, 64 options", bench_decide(&elements, 200)));
    rows.push(Row::new("gate verdict", "block, deny, review, brand, rate", bench_gate(20_000)));

    // --- the eyes ----------------------------------------------------------
    let mut grabs = Vec::new();
    let mut shot = None;
    for _ in 0..20 {
        let t0 = Instant::now();
        let s = kratos_shot::capture();
        grabs.push(ms(t0.elapsed()));
        if s.is_some() {
            shot = s;
        }
    }
    let geometry = match &shot {
        Some(s) => {
            let (w, h) = s.size();
            format!("{w}x{h} virtual desktop, BitBlt + DIB")
        }
        None => "capture unavailable (no session?)".to_string(),
    };
    rows.push(Row::new("screen capture", geometry, grabs));

    if let Some(s) = &shot {
        let (w, h) = s.size();
        let mut full = Vec::with_capacity(200);
        for _ in 0..200 {
            let t0 = Instant::now();
            let f = s.region(None, kratos_shot::FULL_W, kratos_shot::FULL_H);
            full.push(ms(t0.elapsed()));
            std::hint::black_box(f);
        }
        rows.push(Row::new(
            "full fingerprint",
            format!("{}x{} greyscale from the grab", kratos_shot::FULL_W, kratos_shot::FULL_H),
            full,
        ));

        let bbox = [(w / 4) as i32, (h / 4) as i32, 420, 220];
        let mut region = Vec::with_capacity(200);
        for _ in 0..200 {
            let t0 = Instant::now();
            let f = s.region(Some(bbox), kratos_shot::REGION_N, kratos_shot::REGION_N);
            region.push(ms(t0.elapsed()));
            std::hint::black_box(f);
        }
        rows.push(Row::new(
            "element fingerprint",
            format!("{0}x{0} crop of one control", kratos_shot::REGION_N),
            region,
        ));
    }

    // --- report ------------------------------------------------------------
    if json {
        for r in &rows {
            println!(
                "{}",
                serde_json::json!({
                    "name": r.name, "note": r.note, "n": r.samples.len(),
                    "p50_ms": r.p50(), "p95_ms": r.pct(95.0), "min_ms": r.pct(0.0),
                })
            );
        }
        return;
    }

    let table = render(&rows, &notes);
    print!("{table}");
    if let Some(path) = out {
        let doc = document(&table);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, doc).expect("write the report");
        println!("\nwritten: {}", path.display());
    }
}

fn render(rows: &[Row], notes: &[String]) -> String {
    let mut s = String::new();
    s.push_str("| what | p50 ms | p95 ms | best ms | n | |\n");
    s.push_str("|---|---:|---:|---:|---:|---|\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | **{}** | {} | {} | {} | {} |\n",
            r.name,
            fmt(r.p50()),
            fmt(r.pct(95.0)),
            fmt(r.pct(0.0)),
            r.samples.len(),
            r.note
        ));
    }
    for n in notes {
        s.push_str(&format!("\n{n}\n"));
    }
    s
}

/// The committed report: the table, plus enough context that the numbers mean
/// something to someone who did not run it.
fn document(table: &str) -> String {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    // Written by `kratos-native --bench --out`, inlined here so regenerating
    // this file does not silently drop the live half of the story. Absent is
    // fine: a machine that has never run it says so instead of pretending.
    let native = std::fs::read_to_string("docs/native-benchmarks.md").unwrap_or_else(|_| {
        "_No native measurements on this machine yet. Run the command above._\n".to_string()
    });
    format!(
        "# Benchmarks\n\n\
         Reproduce, on your machine, in one command:\n\n\
         ```bash\n\
         cargo run --release -p kratos-bench -- --out docs/benchmarks.md\n\
         ```\n\n\
         Everything below is kratos' own overhead: the MCP shim, the named pipe, the broker's \
         handling, the gate, the candidate compression and the screen fingerprints. The decision \
         model is the offline mock, so no network and no key are involved and nothing here is \
         measuring somebody else's GPU.\n\n\
         {table}\n\
         ## How to read this\n\n\
         **`mcp tools/list` starts no broker.** A harness lists tools at startup and often again \
         per turn. If that woke a daemon, the resting state of this machine would be \"kratos is \
         running\". It answers from a static table instead, and the benchmark asserts afterwards \
         that no pipe exists.\n\n\
         **Cold start is the first tool call, not the first action.** Nothing is registered at \
         logon and nothing runs until an agent asks for something; this is the price of that \
         choice, paid once per idle period.\n\n\
         **The round trip is the floor under every operation.** An `observe` or an `act` costs \
         this, plus the backend, plus the model. When the total is 500ms, that is where it went.\n\n\
         **The gate is not a latency story.** It is listed because a safety check that cost \
         milliseconds would eventually be made optional, and this one cannot be.\n\n\
         **Screen capture is one grab of the whole virtual desktop, cropped twice.** An earlier \
         build took eight separate grabs per action; both rectangles now come out of one, which \
         is where 407ms became 100ms.\n\n\
         ## Native perception, measured live\n\n\
         Not reproducible the way everything above is: these depend on which windows are open \
         and what is in them. They are here because the claim that matters about          `kratos-native` -- one cached fetch per window instead of a call per property -- is          only meaningful against real applications.\n\n\
         ```bash\n\
         cargo run --release -p kratos-native -- --bench \"Notepad\" \"Settings\" --out          docs/native-benchmarks.md\n\
         ```\n\n\
         {native}\n\
         The Python backend this replaces read 24 elements in 223ms and 168 in 2.1s on the same \
         machine.\n\n\
         ## What is not here\n\n\
         The decision model's round trip (70-500ms, somebody else's hardware). It is reported in \
         the README as an observation from a live desktop, which is what it is -- not a \
         benchmark.\n\n\
         ---\n\n\
         Machine: Windows, {cpus} logical cores, release profile (fat LTO, one codegen unit). \
         Percentiles are nearest-rank: no interpolation, so every figure is a sample that was \
         actually taken.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_samples_that_were_actually_taken() {
        let r = Row::new("x", "", vec![5.0, 1.0, 4.0, 2.0, 3.0]);
        // Nearest-rank, no interpolation: every figure printed is a real
        // measurement, not an average of two.
        assert_eq!(r.pct(0.0), 1.0);
        assert_eq!(r.p50(), 3.0);
        assert_eq!(r.pct(95.0), 5.0);
        assert_eq!(r.pct(100.0), 5.0);
    }

    #[test]
    fn an_empty_row_reports_nothing_rather_than_zero() {
        // A capture that failed must not print 0.000 and read as "instant".
        let r = Row::new("x", "", vec![]);
        assert!(r.p50().is_nan());
        assert_eq!(fmt(r.p50()), "--");
    }

    #[test]
    fn figures_keep_three_significant_digits_across_scales() {
        assert_eq!(fmt(0.0312), "0.031");
        assert_eq!(fmt(7.5512), "7.55");
        assert_eq!(fmt(32.87), "32.9");
        assert_eq!(fmt(431.2), "431");
    }

    #[test]
    fn the_synthetic_page_looks_like_a_real_one() {
        let e = synthetic_elements(64);
        assert_eq!(e.len(), 64);
        // Ids of this shape are the reason options are numbered rather than
        // named: 35 characters of routing plumbing told the model nothing.
        assert!(e[0].id.starts_with("brave:w"));
        assert!(e.iter().all(|x| x.container.is_some() && x.selector.is_some()));
    }
}
