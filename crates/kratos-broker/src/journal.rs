//! The action log: one JSONL line per gate decision and per action.
//!
//! Append-only and never truncated in place, so a UI can tail it while the
//! broker writes. Rotation renames rather than rewrites, for the same reason.
//!
//! Every line carries the `peer_id` of the agent that asked. With several
//! agents on one broker, a log that did not say who did what would be
//! unusable for the one question it exists to answer.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const SCHEMA: u32 = 1;

pub struct Journal {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
}

impl Journal {
    pub fn new(path: PathBuf, max_bytes: u64, keep: u32) -> Self {
        Journal { path, max_bytes: max_bytes.max(64 * 1024), keep: keep.min(50) }
    }

    pub fn from_config(cfg: &kratos_protocol::config::LogConfig) -> Self {
        let path = cfg
            .path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(kratos_protocol::paths::journal_path);
        Journal::new(path, cfg.max_bytes, cfg.keep)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record, stamping `ts` and `schema`.
    ///
    /// Failures are swallowed by design. A full disk or a locked file must not
    /// stop the broker from refusing a dangerous action, and the one thing
    /// worse than an unwritten log line is a kill switch that could not be
    /// consulted because logging panicked.
    pub fn append(&self, mut record: Value) {
        if let Some(obj) = record.as_object_mut() {
            obj.insert("ts".into(), Value::String(now_iso8601()));
            obj.insert("schema".into(), Value::from(SCHEMA));
        }
        let Ok(mut line) = serde_json::to_string(&record) else { return };
        line.push('\n');

        self.rotate_if_needed(line.len() as u64);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn rotate_if_needed(&self, incoming: u64) {
        let Ok(meta) = std::fs::metadata(&self.path) else { return };
        if meta.len() + incoming <= self.max_bytes {
            return;
        }
        // actions.jsonl.4 -> .5 (dropped), ... .1 -> .2, then current -> .1
        for i in (1..self.keep).rev() {
            let from = self.sibling(i);
            let to = self.sibling(i + 1);
            if from.exists() {
                let _ = std::fs::rename(&from, &to);
            }
        }
        let _ = std::fs::rename(&self.path, self.sibling(1));
    }

    fn sibling(&self, n: u32) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    /// The last `n` records, oldest first.
    ///
    /// Reads the current file only. A reader asking for recent activity does
    /// not want to pay for a rotated archive, and `n` is bounded so a UI
    /// cannot ask for a 16 MiB response.
    pub fn tail(&self, n: usize, since_ms: Option<u64>) -> Vec<Value> {
        let n = n.clamp(1, 2000);
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        let cutoff = since_ms.map(|ms| {
            chrono::Utc::now() - chrono::Duration::milliseconds(ms.min(u64::MAX / 2) as i64)
        });
        text.lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| match (&cutoff, v.get("ts").and_then(|t| t.as_str())) {
                (Some(c), Some(ts)) => chrono::DateTime::parse_from_rfc3339(ts)
                    .map(|t| t.with_timezone(&chrono::Utc) >= *c)
                    .unwrap_or(true),
                _ => true,
            })
            .take(n)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kratos-journal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("actions.jsonl")
    }

    #[test]
    fn a_record_gains_a_timestamp_and_a_schema() {
        let j = Journal::new(tmp("stamp"), 1 << 20, 3);
        j.append(json!({"op": "act", "peer_id": "a-1", "status": "executed"}));
        let lines = j.tail(10, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["schema"], json!(SCHEMA));
        assert_eq!(lines[0]["peer_id"], json!("a-1"));
        let ts = lines[0]["ts"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "{ts}");
        assert!(ts.ends_with('Z'), "{ts}");
    }

    #[test]
    fn tail_returns_oldest_first_and_respects_the_limit() {
        let j = Journal::new(tmp("order"), 1 << 20, 3);
        for i in 0..10 {
            j.append(json!({"op": "act", "i": i}));
        }
        let last3 = j.tail(3, None);
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0]["i"], json!(7));
        assert_eq!(last3[2]["i"], json!(9));
    }

    #[test]
    fn tailing_a_missing_file_is_empty_not_an_error() {
        let j = Journal::new(tmp("absent").with_file_name("nothing.jsonl"), 1 << 20, 3);
        assert!(j.tail(10, None).is_empty());
    }

    #[test]
    fn rotation_renames_and_keeps_the_configured_number() {
        let path = tmp("rotate");
        // 2 KiB cap, clamped up to the 64 KiB floor, so write past that.
        let j = Journal::new(path.clone(), 0, 3);
        let filler = "x".repeat(400);
        for i in 0..400 {
            j.append(json!({"i": i, "pad": filler}));
        }
        assert!(path.exists(), "the live file must always exist");
        let first = path.with_extension("jsonl.1");
        assert!(first.exists(), "a rotated file should have appeared");
        // Nothing beyond `keep` survives.
        assert!(!path.with_extension("jsonl.4").exists());
        // And the current file is still valid JSONL after rotation.
        let lines = j.tail(5, None);
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.get("i").is_some()));
    }

    #[test]
    fn a_malformed_line_does_not_break_the_tail() {
        let path = tmp("mixed");
        let j = Journal::new(path.clone(), 1 << 20, 3);
        j.append(json!({"i": 1}));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{ truncated\n")
            .unwrap();
        j.append(json!({"i": 2}));
        let lines = j.tail(10, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["i"], json!(2));
    }

    #[test]
    fn an_unwritable_path_is_swallowed_rather_than_fatal() {
        // A log that panics takes the kill switch down with it.
        let j = Journal::new(PathBuf::from("Z:\\no-such-volume\\x\\actions.jsonl"), 1 << 20, 3);
        j.append(json!({"op": "act"}));
        assert!(j.tail(1, None).is_empty());
    }

    #[test]
    fn since_filters_by_timestamp() {
        let j = Journal::new(tmp("since"), 1 << 20, 3);
        j.append(json!({"i": 1}));
        assert_eq!(j.tail(10, Some(60_000)).len(), 1, "just written, inside the window");
        assert_eq!(j.tail(10, Some(0)).len(), 0, "a zero window excludes the past");
    }
}
