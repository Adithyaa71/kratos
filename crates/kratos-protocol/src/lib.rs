//! The wire vocabulary of kratos, as locked in `docs/protocol.md`.
//!
//! This crate is pure data: no I/O, no Windows, no tokio. Every other
//! component -- broker, MCP shim, native host, UI -- shares these types, so a
//! change here is a change to the protocol and shows up as a compile error in
//! each peer rather than as a runtime surprise in one of them.
//!
//! Closed sets (`error.code`, `status`, `role`) are modelled the way the
//! compatibility rules in the doc require: `role` is an enum because an
//! unknown role must be rejected, while `code` and `status` are strings with
//! constants because an older peer must tolerate a member added by a newer
//! one instead of failing to parse the frame.

pub mod config;
pub mod paths;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Protocol major this build speaks.
pub const V: u16 = 1;

/// A line longer than this is a protocol violation (section 1).
pub const MAX_LINE: usize = 8 * 1024 * 1024;

/// Chromium's native-messaging ceiling for a single inbound message.
pub const NATIVE_MSG_MAX: usize = 1024 * 1024;

/// Fragment size the extension chunks above (section 6.3).
pub const CHUNK_THRESHOLD: usize = 768 * 1024;

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// One line on the wire.
///
/// `v` is flattened alongside an internally tagged body so the JSON is exactly
/// `{"v":1,"t":"req",...}` -- one object, no nesting, as documented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub v: u16,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Body {
    Req {
        id: String,
        op: String,
        #[serde(default = "empty_obj")]
        payload: Value,
    },
    Res {
        id: String,
        ok: bool,
        #[serde(default = "empty_obj")]
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WireError>,
    },
    Evt {
        op: String,
        #[serde(default = "empty_obj")]
        payload: Value,
    },
}

fn empty_obj() -> Value {
    json!({})
}

impl Frame {
    pub fn req(id: impl Into<String>, op: impl Into<String>, payload: Value) -> Self {
        Frame { v: V, body: Body::Req { id: id.into(), op: op.into(), payload } }
    }

    pub fn ok(id: impl Into<String>, payload: Value) -> Self {
        Frame { v: V, body: Body::Res { id: id.into(), ok: true, payload, error: None } }
    }

    pub fn err(id: impl Into<String>, e: WireError) -> Self {
        Frame {
            v: V,
            body: Body::Res { id: id.into(), ok: false, payload: empty_obj(), error: Some(e) },
        }
    }

    pub fn evt(op: impl Into<String>, payload: Value) -> Self {
        Frame { v: V, body: Body::Evt { op: op.into(), payload } }
    }

    /// The request/event op, or `None` for a response.
    pub fn op(&self) -> Option<&str> {
        match &self.body {
            Body::Req { op, .. } | Body::Evt { op, .. } => Some(op),
            Body::Res { .. } => None,
        }
    }

    /// The correlation id, or `None` for an event.
    pub fn id(&self) -> Option<&str> {
        match &self.body {
            Body::Req { id, .. } | Body::Res { id, .. } => Some(id),
            Body::Evt { .. } => None,
        }
    }

    pub fn payload(&self) -> &Value {
        match &self.body {
            Body::Req { payload, .. } | Body::Res { payload, .. } | Body::Evt { payload, .. } => {
                payload
            }
        }
    }

    /// Deserialize this frame's payload into a typed request/response struct.
    pub fn parse<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(self.payload())
    }

    /// A response's error, if it carries one.
    pub fn wire_error(&self) -> Option<&WireError> {
        match &self.body {
            Body::Res { error, .. } => error.as_ref(),
            _ => None,
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(&self.body, Body::Res { ok: true, .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
    #[serde(default = "empty_obj", skip_serializing_if = "Value::is_null")]
    pub detail: Value,
    /// Whether sending this request again is safe.
    ///
    /// The question an agent actually has after a failure is not *what went
    /// wrong* but *may I try that again* -- and for anything that moves a
    /// mouse the honest answer is sometimes no, because the input may already
    /// have reached the application. Saying so here means every client gets
    /// the same answer instead of each one guessing from the code.
    #[serde(default = "yes")]
    pub retry_safe: bool,
}

impl WireError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        let retry_safe = retry_is_safe(code);
        WireError { code: code.to_string(), message: message.into(), detail: empty_obj(), retry_safe }
    }

    pub fn with_detail(code: &str, message: impl Into<String>, detail: Value) -> Self {
        let retry_safe = retry_is_safe(code);
        WireError { code: code.to_string(), message: message.into(), detail, retry_safe }
    }

    /// Mark this failure as one where input may already have been delivered.
    ///
    /// Used where the broker knows more than the code does: a backend that
    /// timed out mid-dispatch has a different answer from one that refused the
    /// request outright, even though both can surface as `backend_error`.
    pub fn unsafe_to_retry(mut self) -> Self {
        self.retry_safe = false;
        self
    }
}

/// Whether a failure with this code can be retried without repeating an
/// effect that already happened.
///
/// The rule is about delivery, not about blame: a request the broker rejected
/// never reached a screen and is always safe to send again, while anything
/// that may have been handed to a backend is not.
pub fn retry_is_safe(code: &str) -> bool {
    !matches!(code, codes::BACKEND_TIMEOUT | codes::INTERNAL)
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

/// The closed set of `error.code` values (section 7).
pub mod codes {
    pub const BAD_VERSION: &str = "bad_version";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const TOO_LARGE: &str = "too_large";
    pub const NO_BACKEND: &str = "no_backend";
    pub const NO_TARGET: &str = "no_target";
    /// A `window` argument matched windows of more than one application.
    /// Safe to retry with a more specific string; nothing was dispatched.
    pub const AMBIGUOUS_WINDOW: &str = "ambiguous_window";
    pub const BACKEND_ERROR: &str = "backend_error";
    pub const BACKEND_TIMEOUT: &str = "backend_timeout";
    pub const STALE_ID: &str = "stale_id";
    pub const STALE_CONFIRM: &str = "stale_confirm";
    pub const GATE_BLOCKED: &str = "gate_blocked";
    pub const KILL_SWITCH: &str = "kill_switch";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const LEASE_HELD: &str = "lease_held";
    pub const LEASE_INVALID: &str = "lease_invalid";
    pub const DECIDER_ERROR: &str = "decider_error";
    pub const DECIDER_UNCONFIGURED: &str = "decider_unconfigured";
    pub const INTERNAL: &str = "internal";
}

/// The closed set of `act` outcomes. Not errors -- these ride on `ok:true`.
pub mod status {
    pub const EXECUTED: &str = "executed";
    pub const NO_EFFECT: &str = "no_effect";
    pub const PLANNED: &str = "planned";
    pub const NEEDS_CONFIRMATION: &str = "needs_confirmation";
    pub const REFUSED: &str = "refused";
    pub const BLOCKED: &str = "blocked";
    pub const KILLED: &str = "killed";
}

/// Operation names, so a typo is a compile error rather than a `bad_request`.
pub mod ops {
    pub const HELLO: &str = "hello";
    pub const OBSERVE: &str = "observe";
    pub const ACT: &str = "act";
    pub const VERIFY: &str = "verify";
    pub const CONFIRM: &str = "confirm";
    pub const LEASE_ACQUIRE: &str = "lease.acquire";
    pub const LEASE_RELEASE: &str = "lease.release";
    pub const KILL_ENGAGE: &str = "kill.engage";
    pub const KILL_RESET: &str = "kill.reset";
    pub const KILL_STATUS: &str = "kill.status";
    pub const STATUS: &str = "status";
    pub const LOG_TAIL: &str = "log.tail";
    pub const CONFIG_GET: &str = "config.get";
    pub const CONFIG_SET: &str = "config.set";
    pub const SHUTDOWN: &str = "shutdown";

    pub const BACKEND_OBSERVE: &str = "backend.observe";
    pub const BACKEND_ACT: &str = "backend.act";

    pub const EVT_KILL: &str = "kill";
    pub const EVT_SUPERSEDED: &str = "superseded";
}

// ---------------------------------------------------------------------------
// Roles and handshake
// ---------------------------------------------------------------------------

/// A closed set: an unknown role is rejected, never defaulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Agent,
    Backend,
    Ui,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Backend => "backend",
            Role::Ui => "ui",
        }
    }

    /// Whether this role may invoke `op`. The table in section 3, in code.
    ///
    /// `kill.reset` is absent from `Agent` on purpose: an agent that can clear
    /// its own kill switch does not have a kill switch.
    pub fn may(self, op: &str) -> bool {
        use ops::*;
        match self {
            Role::Agent => matches!(
                op,
                HELLO | OBSERVE | ACT | VERIFY | CONFIRM | LEASE_ACQUIRE | LEASE_RELEASE
                    | KILL_ENGAGE | KILL_STATUS | STATUS | LOG_TAIL
            ),
            Role::Backend => matches!(op, HELLO | STATUS),
            Role::Ui => matches!(
                op,
                HELLO | STATUS | LOG_TAIL | CONFIG_GET | CONFIG_SET | KILL_ENGAGE | KILL_RESET
                    | KILL_STATUS | SHUTDOWN | OBSERVE
            ),
        }
    }
}

/// Which perception/actuation backend a peer provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    DomExt,
    Uia,
    Ocr,
    Pixel,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::DomExt => "dom-ext",
            Source::Uia => "uia",
            Source::Ocr => "ocr",
            Source::Pixel => "pixel",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "dom-ext" => Some(Source::DomExt),
            "uia" => Some(Source::Uia),
            "ocr" => Some(Source::Ocr),
            "pixel" => Some(Source::Pixel),
            _ => None,
        }
    }
}

/// How the broker classified the target window (section 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetClass {
    Browser,
    BrowserAutomation,
    Native,
}

impl TargetClass {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetClass::Browser => "browser",
            TargetClass::BrowserAutomation => "browser-automation",
            TargetClass::Native => "native",
        }
    }

    /// Routing preference, in order. `dom-ext` leads for browsers because it
    /// is the only backend that reports `container` and `selector`.
    pub fn preference(self) -> &'static [Source] {
        match self {
            TargetClass::Browser => &[Source::DomExt, Source::Uia, Source::Pixel],
            TargetClass::BrowserAutomation => &[Source::DomExt, Source::Uia],
            TargetClass::Native => &[Source::Uia, Source::Pixel, Source::Ocr],
        }
    }
}

/// Which *instance* of a backend source this peer is.
///
/// One `dom-ext` source has as many instances as there are browser profiles
/// open, and they are not interchangeable: a tab in one profile cannot be
/// reached through another. The registry is therefore keyed on
/// `(source, instance.token)` rather than on `source` alone -- before that, a
/// second browser profile attaching **evicted the first**, so one harness
/// opening its browser silently unplugged another's.
///
/// `token` is generated by the extension once per profile and stored in that
/// profile, so it survives a reload, a restart and a browser upgrade, and is
/// the only thing here that is stable. Everything else is a hint:
///
/// * `brand` is what the extension detected (`navigator.brave`, the UA), used
///   to narrow which browser a token belongs to.
/// * `pid` is the browser process, reported by the native host as its own
///   parent. It is what links an instance to the windows on screen.
/// * `parent_hwnd` is left absent by `kratos-host`, which has no window of its
///   own; the broker resolves windows from `pid` instead of trusting a guess.
///
/// `profile_dir` is deliberately **not** sent: no extension API reports the
/// profile directory name, so the pairing of a token to a profile is made by
/// the user in the settings window, or by the launch that opened it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_hwnd: Option<i64>,
}

impl Instance {
    pub fn new(token: impl Into<String>) -> Instance {
        Instance { token: token.into(), ..Instance::default() }
    }

    /// A token that is only whitespace is no token at all, and keying the
    /// registry on one would merge two instances into a single slot.
    pub fn usable(&self) -> bool {
        !self.token.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub role: Role,
    pub name: String,
    #[serde(default)]
    pub build: String,
    #[serde(default = "one")]
    pub v_min: u16,
    #[serde(default = "one")]
    pub v_max: u16,
    /// Required when `role` is `backend`.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub handles: Option<Vec<String>>,
    /// Optional, `backend` only. Absent means "the only instance of this
    /// source", which is how every backend behaved before this field existed
    /// and is still correct for `uia`, `ocr` and `pixel`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<Instance>,
}

fn one() -> u16 {
    1
}

impl Hello {
    pub fn agent(name: impl Into<String>, build: impl Into<String>) -> Hello {
        Hello::new(Role::Agent, name, build)
    }

    pub fn new(role: Role, name: impl Into<String>, build: impl Into<String>) -> Hello {
        Hello {
            role,
            name: name.into(),
            build: build.into(),
            v_min: V,
            v_max: V,
            source: None,
            handles: None,
            instance: None,
        }
    }

    pub fn backend(
        name: impl Into<String>,
        build: impl Into<String>,
        source: impl Into<String>,
        handles: Vec<String>,
    ) -> Hello {
        Hello {
            source: Some(source.into()),
            handles: Some(handles),
            ..Hello::new(Role::Backend, name, build)
        }
    }

    pub fn with_instance(mut self, instance: Option<Instance>) -> Hello {
        self.instance = instance.filter(|i| i.usable());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
    pub v: u16,
    pub peer_id: String,
    pub broker: String,
    pub decider: String,
    pub kill_engaged: Vec<String>,
    pub backends: Vec<BackendInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendInfo {
    pub source: String,
    pub handles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    /// Which instance of that source this entry is. Absent for a backend that
    /// sent no `instance`, so a single-instance source reads exactly as it did
    /// before. Two entries with the same `source` are two live instances, not
    /// a duplicate to be cleaned up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<Instance>,
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObserveReq {
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub prefer: Option<String>,
    #[serde(default = "default_observe_timeout")]
    pub timeout_ms: u64,
}

fn default_observe_timeout() -> u64 {
    8_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub gen: u64,
    pub source: String,
    pub window: String,
    #[serde(default)]
    pub url: Option<String>,
    pub target_class: String,
    pub elements: Vec<Element>,
    #[serde(default)]
    pub truncated: bool,
    pub took_ms: u64,
}

/// What a backend reports before the broker stamps a generation on it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendObservation {
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default)]
    pub truncated: bool,
}

/// One interactive thing on screen.
///
/// `container` and `selector` are the fields that exist to stop a decision
/// model from confusing two identically labelled controls. `bbox` is for
/// backends that act by position; `dom-ext` must ignore it when acting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Element {
    pub id: String,
    pub role: String,
    pub label: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// `[x, y, w, h]` in physical screen pixels.
    #[serde(default)]
    pub bbox: Option<[i32; 4]>,
    #[serde(default)]
    pub container: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    /// Set by a backend when the control masks input, so the journal redacts.
    #[serde(default)]
    pub sensitive: bool,
}

fn yes() -> bool {
    true
}

/// Hand-written so it agrees with the serde default above.
///
/// A derived `Default` would give `enabled: false` while a deserialized
/// element with no `enabled` field gets `true` -- two different answers to the
/// same question depending on how the value was built. Every element
/// constructed in Rust would silently read as disabled, and a diff that keys
/// on `enabled` would miss real state changes. Caught by a test that built its
/// fixtures with `..Default::default()`.
impl Default for Element {
    fn default() -> Self {
        Element {
            id: String::new(),
            role: String::new(),
            label: String::new(),
            value: None,
            enabled: true,
            bbox: None,
            container: None,
            selector: None,
            sensitive: false,
        }
    }
}

impl Element {
    /// How the element is presented to the decision model. Including the
    /// container is the point: "textbox \"To\"" is ambiguous on a Gmail
    /// compose, "textbox \"To\" in dialog:New Message" is not.
    pub fn criterion(&self) -> String {
        let label = if self.label.trim().is_empty() { "(unlabelled)" } else { self.label.trim() };
        match &self.container {
            Some(c) if !c.trim().is_empty() => {
                format!("{} \"{}\" in {}", self.role, label, c.trim())
            }
            _ => format!("{} \"{}\"", self.role, label),
        }
    }

    pub fn center(&self) -> Option<(i32, i32)> {
        self.bbox.map(|[x, y, w, h]| (x + w / 2, y + h / 2))
    }
}

// ---------------------------------------------------------------------------
// Action
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActReq {
    pub instruction: String,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub lease: Option<String>,
    /// Pin the target instead of letting the model choose it.
    ///
    /// An id from a recent `observe`. The broker still re-observes, still
    /// decides the *kind*, and still gates -- but the element is the agent's
    /// choice, and an id that is no longer on screen is `stale_id` rather than
    /// a nearby guess. This is the deterministic escape hatch for a screen
    /// where two controls genuinely read alike.
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default = "default_act_timeout")]
    pub timeout_ms: u64,
}

fn default_act_timeout() -> u64 {
    20_000
}

/// The action kinds the decision model chooses between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Click,
    Type,
    Key,
    Scroll,
    Select,
    Navigate,
    Wait,
    Open,
    Focus,
    None,
}

impl Kind {
    pub const ALL: [Kind; 10] = [
        Kind::Click,
        Kind::Type,
        Kind::Key,
        Kind::Scroll,
        Kind::Select,
        Kind::Navigate,
        Kind::Wait,
        Kind::Open,
        Kind::Focus,
        Kind::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Click => "click",
            Kind::Type => "type",
            Kind::Key => "key",
            Kind::Scroll => "scroll",
            Kind::Select => "select",
            Kind::Navigate => "navigate",
            Kind::Wait => "wait",
            Kind::Open => "open",
            Kind::Focus => "focus",
            Kind::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether this kind changes the world, and so must prove it did
    /// (section 5.4) before being called `executed`.
    pub fn is_mutating(self) -> bool {
        !matches!(self, Kind::Wait | Kind::None)
    }

    /// Whether this kind needs an element to aim at. `navigate`, `open` and
    /// `wait` are addressed by name or not at all, so a screen with zero
    /// readable elements must not block them.
    pub fn needs_target(self) -> bool {
        matches!(
            self,
            Kind::Click | Kind::Type | Kind::Select | Kind::Scroll | Kind::Focus
        )
    }

    /// What the model is told each kind means. Written to be mutually
    /// exclusive, and to say out loud where the literal text comes from --
    /// the model picks the action, never the payload.
    pub fn description(self) -> &'static str {
        match self {
            Kind::Click => "Press a button, link, menu item or checkbox once with the mouse",
            Kind::Type => {
                "Enter literal characters into a text field; the exact text is already quoted in \
                 the instruction"
            }
            Kind::Key => {
                "Press a single named keyboard key or shortcut such as Enter, Escape or Ctrl+S, \
                 with no text to type"
            }
            Kind::Scroll => "Move the content of a pane or list up or down without clicking",
            Kind::Select => "Choose one option from a dropdown, combo box or list of choices",
            Kind::Navigate => {
                "Go directly to a URL or website address; the exact address is in the instruction"
            }
            Kind::Wait => "Do nothing and let the screen finish loading or settling",
            Kind::Open => {
                "Launch an application that is not currently in front; the application name is in \
                 the instruction"
            }
            Kind::Focus => "Bring an already-running window to the front without clicking in it",
            Kind::None => {
                "The instruction does not describe any action that can be taken on this screen"
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub kind: String,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub target_label: Option<String>,
    pub confidence: f64,
    pub kind_confidence: f64,
    #[serde(default)]
    pub target_confidence: Option<f64>,
    pub threshold: f64,
    pub decider: String,
    pub decide_ms: u64,
    /// The options the model could not separate, when two or more scored
    /// within noise of each other.
    ///
    /// A low confidence can mean two different things, and they have different
    /// fixes: *nothing here looks right* is answered by picking a different
    /// step, while *two of these look identical* is answered by naming the
    /// container. Folding both into one number told the agent to try harder
    /// when what it needed was to be more specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ambiguous_with: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GateReport {
    pub category: String,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub scope_checked: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StructuralDiff {
    pub added_count: usize,
    pub removed_count: usize,
    pub changed_count: usize,
    pub focus_changed: bool,
}

impl StructuralDiff {
    pub fn touched(&self) -> bool {
        self.added_count > 0
            || self.removed_count > 0
            || self.changed_count > 0
            || self.focus_changed
    }
}

/// The mechanical evidence that an action did something (section 5.4).
///
/// Every pixel field is an `Option`: `None` means "could not look", which must
/// never be read as "nothing happened".
///
/// The `*_baseline` fields carry the ambient motion measured in the same frame
/// immediately before acting. They are what make the pixel numbers
/// interpretable: a delta of 0.195 is decisive on a still screen and
/// meaningless on one with a terminal scrolling in it, and both were measured
/// on this machine. A reader that ignores the baseline will over-read the
/// delta.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Effect {
    pub structural: StructuralDiff,
    /// Coarse delta over the target window's rectangle.
    #[serde(default)]
    pub window_changed: Option<f64>,
    #[serde(default)]
    pub window_baseline: Option<f64>,
    /// Coarse delta over the whole primary monitor. Often `null`, because the
    /// window-scoped measure is the one that excludes unrelated motion.
    #[serde(default)]
    pub screen_changed: Option<f64>,
    /// Fine delta over the target element's rectangle.
    #[serde(default)]
    pub region_changed: Option<f64>,
    #[serde(default)]
    pub region_baseline: Option<f64>,
    #[serde(default)]
    pub readback: Option<String>,
    /// Which sources agreed something happened: a `+`-joined list drawn from
    /// `structural`, `readback`, `window`, `region`, or the single value
    /// `none` / `inconclusive`.
    pub evidence: String,
    /// How long the broker waited for the screen to move after dispatching,
    /// in milliseconds. Reported because it is the largest controllable cost
    /// in an action and the only honest way to show that the wait is now
    /// earned rather than fixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_ms: Option<u64>,
    /// True when the wait ended because something moved, false when it ended
    /// because the ceiling was reached. A `false` here beside an `executed`
    /// means the structure or a read-back carried the proof, not the pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActRes {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<Effect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// `semantic` or `physical`: see [`BackendActRes::method`]. Present on
    /// anything that was dispatched, and recorded in the journal, so a run of
    /// physical actions is visible afterwards rather than inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
    /// Which kind of refusal this is, for an agent that branches on it:
    /// `ambiguous` (two targets scored alike -- name the container),
    /// `unsure` (nothing scored well -- describe it differently),
    /// `no_such_action` (the screen does not offer this -- pick another step).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// Plain-language account of the outcome, for the agent to act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the agent should do next. Present on every non-`executed` status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    pub took_ms: u64,
}

impl ActRes {
    pub fn new(status: &str, took_ms: u64) -> Self {
        ActRes {
            status: status.to_string(),
            gen: None,
            decision: None,
            gate: None,
            effect: None,
            backend: None,
            method: None,
            confirm_token: None,
            expires_in_ms: None,
            refusal: None,
            detail: None,
            next: None,
            took_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Verify, confirm, lease, kill
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VerifyReq {
    pub claim: String,
    #[serde(default)]
    pub window: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRes {
    pub holds: bool,
    pub confidence: f64,
    pub decider: String,
    pub took_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfirmReq {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LeaseAcquireReq {
    #[serde(default = "input_scope")]
    pub scope: String,
    #[serde(default = "default_lease_ttl")]
    pub ttl_ms: u64,
    #[serde(default)]
    pub wait_ms: u64,
}

fn input_scope() -> String {
    "input".to_string()
}

fn default_lease_ttl() -> u64 {
    15_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub token: String,
    pub scope: String,
    pub expires_in_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LeaseReleaseReq {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KillReq {
    #[serde(default = "global_scope")]
    pub scope: String,
    #[serde(default)]
    pub reason: Option<String>,
}

fn global_scope() -> String {
    "global".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillStatus {
    pub engaged: Vec<String>,
    pub hotkey: String,
    pub hotkey_scope: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogTailReq {
    #[serde(default = "default_tail")]
    pub n: usize,
    #[serde(default)]
    pub since_ms: Option<u64>,
}

fn default_tail() -> usize {
    100
}

// ---------------------------------------------------------------------------
// Backend operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendObserveReq {
    #[serde(default)]
    pub window: Option<String>,
    pub class: String,
    #[serde(default = "default_observe_timeout")]
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActTarget {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub bbox: Option<[i32; 4]>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendActReq {
    pub kind: String,
    #[serde(default)]
    pub target: Option<ActTarget>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Scroll ticks, signed: negative is up.
    #[serde(default)]
    pub amount: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendActRes {
    /// `false` with `ok:true` means dispatched and not taken by the page --
    /// the signal that triggers a backend fallback rather than a retry.
    #[serde(default = "yes")]
    pub applied: bool,
    #[serde(default)]
    pub readback: Option<String>,
    /// How the action was delivered: `semantic` through the control's own
    /// interface -- a UI Automation pattern, a DOM event -- or `physical`
    /// through the mouse and keyboard at a position.
    ///
    /// A backend that cannot find a pattern and clicks the middle of the
    /// element instead has done something meaningfully different from what was
    /// asked: it has aimed at a rectangle, and a rectangle can have moved. That
    /// is allowed, because refusing would make half the desktop undriveable,
    /// but it is not allowed to be invisible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub took_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- hello and instances ------------------------------------------------

    #[test]
    fn a_hello_without_an_instance_does_not_mention_one() {
        // An older peer's hello must look exactly as it always did, or every
        // backend that sends no instance becomes a new shape to handle.
        let v = serde_json::to_value(Hello::backend(
            "fake",
            "test",
            "uia",
            vec!["native".into()],
        ))
        .unwrap();
        assert!(v.get("instance").is_none(), "{v}");
        assert_eq!(v["source"], serde_json::json!("uia"));
        assert_eq!(v["v_min"], serde_json::json!(V));
    }

    #[test]
    fn an_instance_round_trips_and_a_blank_token_is_dropped() {
        let hello = Hello::backend("ext", "test", "dom-ext", vec!["browser".into()])
            .with_instance(Some(Instance {
                token: "tok-1".into(),
                brand: Some("brave".into()),
                pid: Some(4242),
                parent_hwnd: None,
            }));
        let v = serde_json::to_value(&hello).unwrap();
        assert_eq!(v["instance"]["token"], serde_json::json!("tok-1"));
        assert_eq!(v["instance"]["pid"], serde_json::json!(4242));
        // Absent, not null: a field that is not known is not a field set to
        // nothing.
        assert!(v["instance"].get("parent_hwnd").is_none());
        let back: Hello = serde_json::from_value(v).unwrap();
        assert_eq!(back.instance, hello.instance);

        // A token of whitespace would key the registry on an empty string and
        // let two instances share one slot.
        let blank = Hello::backend("ext", "test", "dom-ext", vec![])
            .with_instance(Some(Instance::new("   ")));
        assert!(blank.instance.is_none());
    }

    #[test]
    fn an_older_broker_ignores_an_instance_it_does_not_know() {
        // Compatibility rule 3, from the other side: a hello carrying fields
        // this build has never heard of still parses.
        let raw = serde_json::json!({
            "role": "backend", "name": "ext", "build": "t",
            "source": "dom-ext", "handles": ["browser"],
            "instance": {"token": "t1", "brand": "brave", "profile_dir": "Default",
                         "something_new": 7},
            "session": "s-1"
        });
        let hello: Hello = serde_json::from_value(raw).unwrap();
        assert_eq!(hello.instance.unwrap().token, "t1");
    }

    #[test]
    fn a_rejected_request_may_be_retried_and_a_half_delivered_one_may_not() {
        // The question is delivery, not blame. Anything the broker turned away
        // never reached a screen.
        for code in [
            codes::BAD_REQUEST,
            codes::UNAUTHORIZED,
            codes::GATE_BLOCKED,
            codes::KILL_SWITCH,
            codes::RATE_LIMITED,
            codes::NO_BACKEND,
            codes::NO_TARGET,
            codes::DECIDER_ERROR,
            codes::LEASE_HELD,
        ] {
            assert!(retry_is_safe(code), "{code} should be safe to retry");
            assert!(WireError::new(code, "x").retry_safe);
        }
        // A backend that stopped answering mid-dispatch may already have moved
        // the mouse; sending it again would do it twice.
        for code in [codes::BACKEND_TIMEOUT, codes::INTERNAL] {
            assert!(!retry_is_safe(code), "{code} must not be marked retryable");
            assert!(!WireError::new(code, "x").retry_safe);
        }
    }

    #[test]
    fn retry_safety_survives_the_wire_and_can_be_overridden() {
        let e = WireError::new(codes::BACKEND_ERROR, "the page refused").unsafe_to_retry();
        let line = serde_json::to_string(&e).unwrap();
        let back: WireError = serde_json::from_str(&line).unwrap();
        assert!(!back.retry_safe);

        // An old peer that does not send the field must not be read as unsafe:
        // the default is the permissive one, and the broker overrides it.
        let old: WireError =
            serde_json::from_str(r#"{"code":"bad_request","message":"x","detail":{}}"#).unwrap();
        assert!(old.retry_safe);
    }

    #[test]
    fn a_refusal_names_its_kind_and_a_dispatch_names_its_method() {
        let mut res = ActRes::new(status::REFUSED, 12);
        res.refusal = Some("ambiguous".into());
        let v = serde_json::to_value(&res).unwrap();
        assert_eq!(v["refusal"], "ambiguous");
        // Absent fields stay absent: a result that dispatched nothing must not
        // claim a delivery method.
        assert!(v.get("method").is_none());

        let mut done = ActRes::new(status::EXECUTED, 12);
        done.method = Some("physical".into());
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["method"], "physical");
        assert!(v.get("refusal").is_none());
    }

    #[test]
    fn the_settle_wait_is_reported_with_whether_it_ended_early() {
        let e = Effect { settle_ms: Some(96), settled: Some(true), ..Default::default() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["settle_ms"], 96);
        assert_eq!(v["settled"], true);
        // `settled: false` beside an executed status means the pixels never
        // moved and something else carried the proof -- so it has to survive.
        let back: Effect = serde_json::from_value(v).unwrap();
        assert_eq!(back.settle_ms, Some(96));
    }

    #[test]
    fn request_round_trips_byte_for_byte() {
        let f = Frame::req("7f3a9c", ops::ACT, json!({"instruction": "click the \"Send\" button"}));
        let line = serde_json::to_string(&f).unwrap();
        assert_eq!(
            line,
            r#"{"v":1,"t":"req","id":"7f3a9c","op":"act","payload":{"instruction":"click the \"Send\" button"}}"#
        );
        let back: Frame = serde_json::from_str(&line).unwrap();
        assert_eq!(back.op(), Some("act"));
        assert_eq!(back.id(), Some("7f3a9c"));
    }

    #[test]
    fn ok_response_never_carries_an_error_field() {
        let line = serde_json::to_string(&Frame::ok("1", json!({"a": 1}))).unwrap();
        assert!(!line.contains("error"), "{line}");
        assert_eq!(line, r#"{"v":1,"t":"res","id":"1","ok":true,"payload":{"a":1}}"#);
    }

    #[test]
    fn error_response_shape() {
        let f = Frame::err("1", WireError::new(codes::GATE_BLOCKED, "denylisted"));
        let line = serde_json::to_string(&f).unwrap();
        let back: Frame = serde_json::from_str(&line).unwrap();
        assert!(!back.is_ok());
        assert_eq!(back.wire_error().unwrap().code, "gate_blocked");
    }

    #[test]
    fn event_has_no_id() {
        let line = serde_json::to_string(&Frame::evt(ops::EVT_KILL, json!({}))).unwrap();
        assert_eq!(line, r#"{"v":1,"t":"evt","op":"kill","payload":{}}"#);
        let back: Frame = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id(), None);
    }

    #[test]
    fn unknown_fields_are_ignored_not_fatal() {
        // Compatibility rule 3: a peer must not fail on a field it has never
        // heard of, which is what lets v1 peers talk to later builds.
        let line = r#"{"v":1,"t":"req","id":"1","op":"status","payload":{},"future":42}"#;
        let f: Frame = serde_json::from_str(line).unwrap();
        assert_eq!(f.op(), Some("status"));
    }

    #[test]
    fn missing_payload_defaults_to_empty_object() {
        let f: Frame = serde_json::from_str(r#"{"v":1,"t":"req","id":"1","op":"status"}"#).unwrap();
        assert_eq!(f.payload(), &json!({}));
    }

    #[test]
    fn agent_cannot_reset_the_kill_switch() {
        assert!(Role::Agent.may(ops::KILL_ENGAGE));
        assert!(!Role::Agent.may(ops::KILL_RESET));
        assert!(Role::Ui.may(ops::KILL_RESET));
        assert!(!Role::Agent.may(ops::CONFIG_SET));
        assert!(!Role::Backend.may(ops::ACT));
    }

    #[test]
    fn element_criterion_disambiguates_by_container() {
        let a = Element {
            role: "textbox".into(),
            label: "To".into(),
            container: Some("dialog:New Message".into()),
            ..Default::default()
        };
        let b = Element { role: "textbox".into(), label: "To".into(), ..Default::default() };
        assert_eq!(a.criterion(), "textbox \"To\" in dialog:New Message");
        assert_eq!(b.criterion(), "textbox \"To\"");
        assert_ne!(a.criterion(), b.criterion());
    }

    #[test]
    fn kinds_round_trip_and_classify() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
            assert!(!k.description().is_empty());
        }
        assert!(Kind::Click.is_mutating());
        assert!(!Kind::Wait.is_mutating());
        assert!(!Kind::None.is_mutating());
        // navigate and open address a target by name, so they must not require
        // a readable element -- an empty screen used to short-circuit them.
        assert!(!Kind::Navigate.needs_target());
        assert!(!Kind::Open.needs_target());
        assert!(Kind::Click.needs_target());
    }

    #[test]
    fn routing_puts_dom_ext_first_for_browsers_only() {
        assert_eq!(TargetClass::Browser.preference()[0], Source::DomExt);
        assert_eq!(TargetClass::Native.preference()[0], Source::Uia);
        assert!(!TargetClass::Native.preference().contains(&Source::DomExt));
    }

    #[test]
    fn source_strings_are_kebab_case_on_the_wire() {
        assert_eq!(serde_json::to_string(&Source::DomExt).unwrap(), "\"dom-ext\"");
        assert_eq!(Source::parse("dom-ext"), Some(Source::DomExt));
        assert_eq!(Source::parse("nope"), None);
    }

    #[test]
    fn the_rust_default_and_the_serde_default_agree() {
        // These are two independent code paths to the same value, and they
        // disagreed once: `enabled` defaulted to false in Rust and true on the
        // wire, so every element built in Rust looked disabled.
        let from_rust = Element::default();
        // id, role and label are required on the wire -- a backend that omits
        // them has not described an element -- so the minimum valid object
        // supplies those and nothing else.
        let from_wire: Element =
            serde_json::from_value(json!({"id": "e1", "role": "button", "label": "Send"})).unwrap();
        assert_eq!(from_rust.enabled, from_wire.enabled);
        assert!(from_rust.enabled, "an element with nothing said about it is usable");
        assert_eq!(from_rust.sensitive, from_wire.sensitive);
        assert_eq!(from_rust.value, from_wire.value);
    }

    #[test]
    fn structural_diff_touched_requires_something() {
        assert!(!StructuralDiff::default().touched());
        assert!(StructuralDiff { focus_changed: true, ..Default::default() }.touched());
        assert!(StructuralDiff { added_count: 1, ..Default::default() }.touched());
    }
}
