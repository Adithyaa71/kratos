//! What every observation must be true of, checked against real windows.
//!
//! The corpus is generated on the machine it is checked on and is **not**
//! committed: a UIA dump carries what is on the operator's screen -- the file
//! open in Notepad, the names in a folder, the subject lines in a mail client.
//! So this skips what is not there and says so, rather than passing silently
//! on an empty directory.
//!
//! ```text
//! cargo run --release -p kratos-native -- --dump "Notepad" > fixtures/uia/notepad.json
//! ```
//!
//! What it does check is the part a later phase can break without noticing:
//! ids unique within an observation, rectangles that are somewhere, roles from
//! one vocabulary, containers that name something. Those are the invariants
//! the decision model is handed, and a fusion pass (P4d) that merges two
//! sources is exactly the sort of change that quietly breaks them.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value;

fn corpus() -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("uia");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let text = decode(&bytes);
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            panic!("{} is not JSON; regenerate it", path.display())
        };
        out.push((path.file_stem().unwrap_or_default().to_string_lossy().to_string(), v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Read a fixture whatever the shell that wrote it decided to encode it as.
///
/// `>` in Windows PowerShell writes UTF-16LE, and `Out-File -Encoding utf8`
/// writes a byte order mark. Both produce a file that is obviously JSON when
/// you look at it and is not JSON to a parser -- the same three invisible
/// bytes that once broke config loading and the MCP handshake here. The
/// operator generating a corpus should not have to know that, so this does.
fn decode(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text)
}

/// The roles the broker's candidate compression and the decision model expect
/// to see. `element` is the deliberate catch-all for a control type nobody has
/// mapped: unmapped is not unknown.
const ROLES: &[&str] = &[
    "button", "calendar", "checkbox", "combobox", "edit", "link", "image", "listitem", "list",
    "menu", "menubar", "menuitem", "progressbar", "radiobutton", "scrollbar", "slider", "spinner",
    "statusbar", "tab", "tabitem", "text", "toolbar", "tooltip", "tree", "treeitem", "custom",
    "group", "thumb", "datagrid", "dataitem", "document", "splitbutton", "window", "pane",
    "header", "headeritem", "table", "titlebar", "separator", "semanticzoom", "appbar", "element",
];

#[test]
fn every_observation_in_the_corpus_holds_its_shape() {
    let corpus = corpus();
    if corpus.is_empty() {
        eprintln!(
            "no fixtures in fixtures/uia; generate some with --dump (see this file's header)"
        );
        return;
    }

    for (name, obs) in &corpus {
        let elements = obs["elements"].as_array().unwrap_or_else(|| {
            panic!("{name}: no elements array")
        });
        assert!(!elements.is_empty(), "{name}: an empty window is not a useful fixture");

        let mut seen: HashSet<&str> = HashSet::new();
        for (i, e) in elements.iter().enumerate() {
            let id = e["id"].as_str().unwrap_or_else(|| panic!("{name}[{i}]: no id"));
            assert!(
                seen.insert(id),
                "{name}: two elements share the id {id:?}. An id that points at two things is \
                 how an action lands on the wrong one"
            );
            assert!(
                id.starts_with("uia:"),
                "{name}[{i}]: {id:?} is not namespaced to the backend that made it"
            );

            let role = e["role"].as_str().unwrap_or("");
            assert!(
                ROLES.contains(&role),
                "{name}[{i}]: role {role:?} is outside the vocabulary the decision model is \
                 given"
            );

            if let Some(bbox) = e["bbox"].as_array() {
                assert_eq!(bbox.len(), 4, "{name}[{i}]: a bbox is x, y, w, h");
                let w = bbox[2].as_i64().unwrap_or(0);
                let h = bbox[3].as_i64().unwrap_or(0);
                assert!(
                    w > 0 && h > 0,
                    "{name}[{i}]: a {w}x{h} rectangle is somewhere to aim that is nowhere"
                );
            }

            if let Some(container) = e["container"].as_str() {
                assert!(
                    container.contains(':') && !container.ends_with(':'),
                    "{name}[{i}]: container {container:?} should read <role>:<name>"
                );
            }
        }
    }
}

/// The claim this crate exists for, checked against whatever is on the
/// machine: the cost is the one cross-process fetch, and the local walk over
/// the cache is free by comparison.
///
/// If this ever inverts, something has started calling across the boundary
/// per element again -- which is the 2.1-second behaviour this replaced.
#[test]
fn the_cost_is_the_fetch_and_not_the_walk() {
    let corpus = corpus();
    if corpus.is_empty() {
        eprintln!("no fixtures in fixtures/uia; nothing to check");
        return;
    }
    for (name, obs) in &corpus {
        let fetch = obs["fetch_ms"].as_u64().unwrap_or(0);
        let walk = obs["walk_ms"].as_u64().unwrap_or(0);
        let count = obs["elements"].as_array().map(|a| a.len()).unwrap_or(0);
        eprintln!("{name}: {count} elements, fetch {fetch}ms, walk {walk}ms");
        assert!(
            walk <= 10 && (walk * 4 <= fetch.max(1) || fetch == 0),
            "{name}: the local walk took {walk}ms against a {fetch}ms fetch. Reading the cache \
             should be free; this looks like a call per element."
        );
    }
}
