//! The on-disk configuration, exactly as section 9 of the protocol specifies.
//!
//! Every field has a default, and loading tolerates a file written by an older
//! build: a missing key takes the default rather than failing the load, which
//! is what keeps a half-edited config from leaving the broker unable to start.
//! A malformed file is reported, not silently replaced -- overwriting someone's
//! hand-edited gate rules because of a stray comma would be worse than
//! refusing to start.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema: u32,
    pub pipe_name: String,
    /// Broker exits after this long without an agent request, provided no UI
    /// is attached. Section 5.5.
    pub idle_exit_ms: u64,
    /// How long to keep looking for a change after dispatching an action
    /// before giving up and calling it `no_effect`.
    ///
    /// This is a ceiling, not a wait. The broker polls the target's pixels and
    /// returns the moment they move, so a responsive UI costs a fraction of
    /// this and an inert control costs all of it. A fixed settle paid the
    /// worst case every time.
    pub settle_max_ms: u64,
    pub decider: Decider,
    pub thresholds: Thresholds,
    pub kill: KillConfig,
    pub gate: GateConfig,
    pub backends: Backends,
    pub log: LogConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            schema: SCHEMA,
            pipe_name: "kratos.v1".into(),
            idle_exit_ms: 300_000,
            settle_max_ms: 1_200,
            decider: Decider::default(),
            thresholds: Thresholds::default(),
            kill: KillConfig::default(),
            gate: GateConfig::default(),
            backends: Backends::default(),
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Decider {
    /// `jev`, or any other registered provider. Replaceable by design: a
    /// provider need only return a label with a distribution and a calibrated
    /// confidence over a fixed criteria set.
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    /// DPAPI-sealed, base64. Never leaves the broker and never enters the log.
    pub api_key_enc: String,
    pub timeout_ms: u64,
}

impl Default for Decider {
    fn default() -> Self {
        Decider {
            provider: "jev".into(),
            model: "jev-latest".into(),
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            api_key_enc: String::new(),
            timeout_ms: 8_000,
        }
    }
}

/// Per-kind confidence bars, applied to the weaker of kind and target
/// confidence. The numbers are the ones measured on the Python build, where
/// executed decisions ran at or above 0.96 and escalated ones at or below
/// 0.65: there is a wide gap to sit a threshold in, and these sit in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    pub click: f64,
    #[serde(rename = "type")]
    pub type_: f64,
    pub key: f64,
    pub scroll: f64,
    pub select: f64,
    pub navigate: f64,
    pub open: f64,
    pub focus: f64,
    pub default: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            click: 0.70,
            type_: 0.75,
            key: 0.65,
            scroll: 0.55,
            select: 0.70,
            navigate: 0.75,
            open: 0.70,
            focus: 0.60,
            default: 0.70,
        }
    }
}

impl Thresholds {
    pub fn for_kind(&self, kind: crate::Kind) -> f64 {
        use crate::Kind::*;
        match kind {
            Click => self.click,
            Type => self.type_,
            Key => self.key,
            Scroll => self.scroll,
            Select => self.select,
            Navigate => self.navigate,
            Open => self.open,
            Focus => self.focus,
            Wait | None => self.default,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KillConfig {
    pub hotkey: String,
    /// What the hotkey engages. `global` halts everything; a brand scope halts
    /// one browser and leaves the rest working.
    pub hotkey_scope: String,
    /// Which browser brands may be scoped in the UI. Does not engage anything.
    pub per_browser: BTreeMap<String, bool>,
}

impl Default for KillConfig {
    fn default() -> Self {
        let mut per_browser = BTreeMap::new();
        for b in ["brave", "chrome", "edge", "vivaldi", "opera"] {
            per_browser.insert(b.to_string(), true);
        }
        KillConfig {
            hotkey: "ctrl+alt+k".into(),
            hotkey_scope: "global".into(),
            per_browser,
        }
    }
}

/// Ported from the Python build's `config/gate.json`, which these lists came
/// from, with the recipient allowlist and the OS-dialog escalation added.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    pub block_keywords: Vec<String>,
    pub review_keywords: Vec<String>,
    pub review_apps: Vec<String>,
    pub review_domains: Vec<String>,
    pub deny_domains: Vec<String>,
    pub deny_apps: Vec<String>,
    /// When non-empty, a send to a recipient outside this list escalates to
    /// review even if no keyword matched.
    pub allow_recipients: Vec<String>,
    /// When non-empty, the only browser brands this broker may drive, by name
    /// (`brave`, `chrome`, `edge`, `vivaldi`, `opera`). Anything else is
    /// blocked outright. Empty means no restriction.
    ///
    /// Matched against the resolved brand -- the process image, or the brand
    /// the extension names itself by in its element ids -- never against a
    /// window title, which on a browser is the page's title and therefore
    /// arbitrary text from the internet.
    pub allowed_browsers: Vec<String>,
    pub rate_limit_per_minute: u32,
    /// Rule *prefixes* whose review cannot be satisfied by an agent's
    /// `confirm` and must be clicked by a human in a native dialog.
    pub require_os_dialog_for: Vec<String>,
    pub review_dialog_timeout_s: u64,
    /// How long a parked `needs_confirmation` token stays valid.
    pub confirm_ttl_ms: u64,
}

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            block_keywords: strs(&[
                "format c:",
                "rm -rf /",
                "del /s /q c:\\",
                "diskpart clean",
                "shutdown /s",
                "factory reset",
                "reset this pc",
            ]),
            review_keywords: strs(&[
                "delete",
                "remove",
                "uninstall",
                "cancel subscription",
                "unsubscribe",
                "send",
                "submit",
                "pay",
                "payment",
                "checkout",
                "buy",
                "purchase",
                "order",
                "transfer",
                "wire",
                "install",
                "download and run",
                "format",
                "disable",
                "wipe",
                "erase",
            ]),
            review_apps: strs(&[
                "1password",
                "lastpass",
                "bitwarden",
                "keychain access",
                "keepass",
                "settings",
                "control panel",
                "registry editor",
                "regedit",
                "task manager",
                "powershell",
                "command prompt",
                "terminal",
            ]),
            review_domains: strs(&[
                "chase.com",
                "bankofamerica.com",
                "wellsfargo.com",
                "citi.com",
                "paypal.com",
                "venmo.com",
                "coinbase.com",
                "binance.com",
                "kraken.com",
            ]),
            deny_domains: vec![],
            deny_apps: vec![],
            allow_recipients: vec![],
            allowed_browsers: vec![],
            rate_limit_per_minute: 30,
            require_os_dialog_for: strs(&["review_domain", "recipient_not_allowed"]),
            review_dialog_timeout_s: 120,
            confirm_ttl_ms: 180_000,
        }
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Backends {
    pub enabled: Vec<String>,
}

impl Default for Backends {
    fn default() -> Self {
        Backends { enabled: strs(&["dom-ext", "uia", "pixel", "ocr"]) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub path: Option<String>,
    pub max_bytes: u64,
    pub keep: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig { path: None, max_bytes: 16 * 1024 * 1024, keep: 5 }
    }
}

#[derive(Debug)]
pub enum LoadError {
    /// The file is not there. The caller takes defaults; this is normal on a
    /// first run and is not an error condition.
    Missing,
    Io(std::io::Error),
    /// Present but unparseable. Deliberately fatal: silently replacing a
    /// hand-edited gate policy because of a stray comma would be worse than
    /// refusing to start.
    Parse(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Missing => write!(f, "no config file"),
            LoadError::Io(e) => write!(f, "{e}"),
            LoadError::Parse(m) => write!(f, "malformed config: {m}"),
        }
    }
}

impl Config {
    pub fn load_from(path: &std::path::Path) -> Result<Config, LoadError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::Missing),
            Err(e) => return Err(LoadError::Io(e)),
        };
        // Strip a UTF-8 byte order mark. PowerShell's `Set-Content
        // -Encoding utf8` writes one, and so does Notepad, and serde reports
        // it as "expected value at line 1 column 1" -- which sends you looking
        // at your JSON rather than at three invisible bytes in front of it.
        // Refusing to start over a BOM is not useful strictness.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        serde_json::from_str(text).map_err(|e| LoadError::Parse(e.to_string()))
    }

    /// Load, or take defaults when absent. A malformed file still errors.
    pub fn load_or_default(path: &std::path::Path) -> Result<Config, LoadError> {
        match Config::load_from(path) {
            Ok(c) => Ok(c),
            Err(LoadError::Missing) => Ok(Config::default()),
            Err(e) => Err(e),
        }
    }

    /// Write atomically: a temp file in the same directory, then a rename, so
    /// a reader never sees a half-written config and a crash mid-write cannot
    /// destroy the previous one.
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// Apply a shallow JSON merge, as `config.set` specifies. Objects are
    /// merged one level deep so a UI can send `{"decider":{"model":"x"}}`
    /// without having to echo back the whole decider block -- and without the
    /// API key it is never given.
    pub fn merge_patch(&self, patch: &serde_json::Value) -> Result<Config, String> {
        let mut base = serde_json::to_value(self).map_err(|e| e.to_string())?;
        merge(&mut base, patch);
        serde_json::from_value(base).map_err(|e| e.to_string())
    }

    /// The config as the UI may see it: the sealed key replaced by a boolean.
    pub fn redacted(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(d) = v.get_mut("decider").and_then(|d| d.as_object_mut()) {
            let present = !self.decider.api_key_enc.is_empty();
            d.remove("api_key_enc");
            d.insert("key_present".into(), serde_json::Value::Bool(present));
        }
        v
    }
}

fn merge(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(b), serde_json::Value::Object(p)) => {
            for (k, pv) in p {
                match b.get_mut(k) {
                    // One level deep: nested objects merge, everything else
                    // (including arrays) is replaced wholesale. A gate list
                    // must be settable to fewer entries than it has now.
                    Some(bv) if bv.is_object() && pv.is_object() => merge(bv, pv),
                    _ => {
                        b.insert(k.clone(), pv.clone());
                    }
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_round_trip_through_json() {
        let c = Config::default();
        let text = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.idle_exit_ms, 300_000);
        // The ceiling on the post-action wait, not the wait itself.
        assert_eq!(back.settle_max_ms, 1_200);
        assert_eq!(back.kill.hotkey, "ctrl+alt+k");
        assert_eq!(back.gate.rate_limit_per_minute, 30);
        assert_eq!(back.thresholds.type_, 0.75);
    }

    #[test]
    fn an_empty_object_loads_as_all_defaults() {
        // A config written by an older build is missing keys the new one
        // knows about; it must still start.
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.idle_exit_ms, 300_000);
        assert_eq!(c.decider.provider, "jev");
        assert!(!c.gate.review_keywords.is_empty());
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let c: Config = serde_json::from_str(r#"{"idle_exit_ms": 60000}"#).unwrap();
        assert_eq!(c.idle_exit_ms, 60_000);
        assert_eq!(c.thresholds.click, 0.70);
    }

    #[test]
    fn type_threshold_serialises_as_type_not_type_underscore() {
        let text = serde_json::to_string(&Thresholds::default()).unwrap();
        assert!(text.contains("\"type\":0.75"), "{text}");
        assert!(!text.contains("type_"), "{text}");
    }

    #[test]
    fn patch_merges_one_level_and_replaces_arrays() {
        let c = Config::default();
        let merged = c
            .merge_patch(&json!({
                "idle_exit_ms": 120000,
                "decider": {"model": "jev-preview"},
                "gate": {"deny_domains": ["example.com"]}
            }))
            .unwrap();
        assert_eq!(merged.idle_exit_ms, 120_000);
        assert_eq!(merged.decider.model, "jev-preview");
        // untouched sibling inside a merged object survives
        assert_eq!(merged.decider.provider, "jev");
        assert_eq!(merged.gate.deny_domains, vec!["example.com".to_string()]);
        // and an array patch replaces rather than appends
        let shrunk = merged.merge_patch(&json!({"gate": {"review_keywords": ["send"]}})).unwrap();
        assert_eq!(shrunk.gate.review_keywords, vec!["send".to_string()]);
    }

    #[test]
    fn redacted_never_exposes_the_key() {
        let mut c = Config::default();
        c.decider.api_key_enc = "c2VjcmV0".into();
        let v = c.redacted();
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains("c2VjcmV0"), "{text}");
        assert!(!text.contains("api_key_enc"), "{text}");
        assert_eq!(v["decider"]["key_present"], json!(true));

        let empty = Config::default().redacted();
        assert_eq!(empty["decider"]["key_present"], json!(false));
    }

    #[test]
    fn a_byte_order_mark_does_not_brick_the_config() {
        // PowerShell and Notepad both write one, and serde blames line 1
        // column 1 -- the wrong thing to go looking at.
        let dir = std::env::temp_dir().join("kratos-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bom.json");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"idle_exit_ms": 4242}"#);
        std::fs::write(&p, bytes).unwrap();

        let loaded = Config::load_from(&p).expect("a BOM is not a syntax error");
        assert_eq!(loaded.idle_exit_ms, 4242);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn malformed_config_is_fatal_not_silently_defaulted() {
        let dir = std::env::temp_dir().join("kratos-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.json");
        std::fs::write(&p, "{ not json").unwrap();
        assert!(matches!(Config::load_or_default(&p), Err(LoadError::Parse(_))));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_is_identity() {
        let dir = std::env::temp_dir().join("kratos-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rt.json");
        let mut c = Config { idle_exit_ms: 1234, ..Default::default() };
        c.decider.api_key_enc = "sealed".into();
        c.save_to(&p).unwrap();
        let back = Config::load_from(&p).unwrap();
        assert_eq!(back.idle_exit_ms, 1234);
        assert_eq!(back.decider.api_key_enc, "sealed");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn thresholds_cover_every_kind() {
        let t = Thresholds::default();
        for k in crate::Kind::ALL {
            let v = t.for_kind(k);
            assert!((0.0..=1.0).contains(&v), "{k:?} -> {v}");
        }
    }
}
