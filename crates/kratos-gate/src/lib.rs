//! The safety gate: every action passes through here before anything moves.
//!
//! This is not the decision model's confidence threshold. That answers "is the
//! model sure what was meant"; this answers "is this allowed to happen at all,
//! and if it is borderline, does a human have to approve it in person". It
//! applies to every agent on the broker, not only to one caller.
//!
//! Ported from the Python build's `decision/gate.py`, with three changes the
//! protocol requires: the kill switch is a set of scopes rather than one
//! boolean, a recipient allowlist can escalate a send on its own, and nothing
//! here blocks -- the native dialog is a separate, explicit call.

pub mod dialog;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use kratos_protocol::config::GateConfig;

/// What the gate decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Allow,
    /// A human must approve. Never self-approvable by the agent when
    /// `requires_os_dialog` is set.
    Review,
    /// Denylisted. No confirmation path exists.
    Block,
    Killed,
    RateLimited,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Allow => "allow",
            Category::Review => "review",
            Category::Block => "block",
            Category::Killed => "kill_switch",
            Category::RateLimited => "rate_limited",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub category: Category,
    pub rule: Option<String>,
    /// When true, an agent's `confirm` is not sufficient: a human must click a
    /// dialog this process owns.
    pub requires_os_dialog: bool,
    pub scope_checked: Option<String>,
}

impl Verdict {
    fn new(category: Category, rule: Option<String>) -> Self {
        Verdict { category, rule, requires_os_dialog: false, scope_checked: None }
    }

    pub fn allow() -> Self {
        Verdict::new(Category::Allow, None)
    }

    pub fn report(&self) -> kratos_protocol::GateReport {
        kratos_protocol::GateReport {
            category: self.category.as_str().to_string(),
            rule: self.rule.clone(),
            scope_checked: self.scope_checked.clone(),
        }
    }
}

/// What is being acted on, resolved by the broker before the gate sees it.
#[derive(Debug, Clone, Default)]
pub struct TargetRef {
    pub window_title: String,
    pub url: Option<String>,
    /// Process image name, lowercase, e.g. `brave.exe`.
    pub exe: Option<String>,
    /// Browser brand, lowercase, e.g. `brave`, when the target is a browser.
    pub brand: Option<String>,
}

impl TargetRef {
    /// The kill scopes that cover this target, most specific first.
    pub fn scopes(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(exe) = &self.exe {
            v.push(format!("app:{}", exe.to_ascii_lowercase()));
        }
        if let Some(brand) = &self.brand {
            v.push(format!("browser:{}", brand.to_ascii_lowercase()));
        }
        v
    }

    pub fn domain(&self) -> Option<String> {
        extract_domain(self.url.as_deref().unwrap_or(&self.window_title))
    }
}

// ---------------------------------------------------------------------------
// Kill switch
// ---------------------------------------------------------------------------

/// A set of engaged scopes, not a mode.
///
/// `browser:brave` and `app:excel.exe` can be engaged at once while everything
/// else keeps working, which is the difference between a kill switch that gets
/// used and one that gets left off because it stops too much.
#[derive(Debug, Clone, Default)]
pub struct KillSwitch {
    engaged: BTreeSet<String>,
}

impl KillSwitch {
    pub fn engage(&mut self, scope: &str) {
        self.engaged.insert(normalise_scope(scope));
    }

    pub fn reset(&mut self, scope: &str) {
        let s = normalise_scope(scope);
        if s == "global" {
            // Clearing global clears everything: a human reaching for "resume"
            // after slamming the hotkey means resume, not resume-except-two.
            self.engaged.clear();
        } else {
            self.engaged.remove(&s);
        }
    }

    pub fn reset_all(&mut self) {
        self.engaged.clear();
    }

    pub fn is_engaged(&self) -> bool {
        !self.engaged.is_empty()
    }

    pub fn engaged(&self) -> Vec<String> {
        self.engaged.iter().cloned().collect()
    }

    /// The engaged scope covering this target, if any.
    pub fn covers(&self, target: &TargetRef) -> Option<String> {
        if self.engaged.contains("global") {
            return Some("global".to_string());
        }
        target.scopes().into_iter().find(|s| self.engaged.contains(s))
    }
}

fn normalise_scope(scope: &str) -> String {
    let s = scope.trim().to_ascii_lowercase();
    if s.is_empty() {
        "global".to_string()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RateLimiter {
    per_minute: u32,
    hits: Vec<Instant>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        RateLimiter { per_minute, hits: Vec::new() }
    }

    pub fn set_limit(&mut self, per_minute: u32) {
        self.per_minute = per_minute;
    }

    pub fn allow(&mut self) -> bool {
        self.allow_at(Instant::now())
    }

    fn allow_at(&mut self, now: Instant) -> bool {
        let minute = Duration::from_secs(60);
        self.hits.retain(|t| now.duration_since(*t) < minute);
        if self.hits.len() as u32 >= self.per_minute {
            return false;
        }
        self.hits.push(now);
        true
    }
}

// ---------------------------------------------------------------------------
// Text analysis
// ---------------------------------------------------------------------------

const TYPING_VERBS: [&str; 6] = ["type", "enter", "write", "input", "paste", "fill"];

/// Whether the instruction is asking for literal text to be entered.
fn is_typing_instruction(instruction: &str) -> bool {
    words(instruction).any(|w| TYPING_VERBS.contains(&w.as_str()))
}

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase())
}

/// Does `haystack` contain `needle` as whole words?
///
/// Substring matching would make `send` fire on `sender`, `resend` and
/// `godsend`; the keywords name actions, so they are matched as words. A
/// multi-word keyword is matched as a contiguous word sequence.
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    let n: Vec<String> = words(needle).collect();
    if n.is_empty() {
        // A keyword with no word characters at all (e.g. "rm -rf /") can only
        // be matched as a raw substring.
        return haystack.to_lowercase().contains(&needle.to_lowercase());
    }
    let h: Vec<String> = words(haystack).collect();
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w == n.as_slice())
}

/// The part of an instruction the review keywords should see.
///
/// The keywords name actions -- send, pay, delete, order. Matching them
/// against the raw string also matches text the agent is merely being asked to
/// type, and then an ordinary email cannot be written: the subject line
/// `type "Order-taking AI agents for distributors" into the Subject field`
/// trips the `order` rule. Found live, drafting exactly that email.
///
/// So for a typing instruction the quoted payload is dropped before matching.
/// Everything else keeps its quotes, because elsewhere the quoted span is the
/// *target*, and `click the "Send" button` must still be caught.
pub fn reviewable_text(instruction: &str) -> String {
    if !is_typing_instruction(instruction) {
        return instruction.to_string();
    }
    strip_quoted(instruction)
}

/// Blank out quoted spans, leaving a space so word boundaries survive.
///
/// A single quote only opens a span when it looks like a quote rather than an
/// apostrophe -- at a word boundary, closed at a word boundary. Without that
/// test, `don't open Bob's settings` would have `t open Bob` removed, which
/// would hide a review keyword inside a contraction. The Python original had
/// exactly that hole.
fn strip_quoted(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let close = match c {
            '"' => Some('"'),
            '\u{201c}' => Some('\u{201d}'),
            '\'' if opens_quote(&chars, i) => Some('\''),
            '\u{2018}' => Some('\u{2019}'),
            _ => None,
        };
        match close {
            Some(closer) => {
                let mut j = i + 1;
                let mut found = None;
                while j < chars.len() {
                    if chars[j] == closer && (closer != '\'' || closes_quote(&chars, j)) {
                        found = Some(j);
                        break;
                    }
                    j += 1;
                }
                match found {
                    Some(end) => {
                        out.push(' ');
                        i = end + 1;
                    }
                    // Unbalanced: keep the character, do not swallow the rest
                    // of the instruction.
                    None => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            None => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn opens_quote(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1).map(|p| chars[p]) {
        None => true,
        Some(p) => p.is_whitespace() || p == '(' || p == '[' || p == ':' || p == ',',
    }
}

fn closes_quote(chars: &[char], i: usize) -> bool {
    match chars.get(i + 1) {
        None => true,
        Some(n) => !n.is_alphanumeric(),
    }
}

/// Email addresses mentioned in a string. Deliberately simple: it only has to
/// find candidates for an allowlist check, not validate RFC 5322.
pub fn emails_in(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in s.split(|c: char| c.is_whitespace() || "\"'<>(),;:[]".contains(c)) {
        let t = token.trim_matches(|c: char| !c.is_alphanumeric());
        if let Some(at) = t.find('@') {
            let (local, domain) = (&t[..at], &t[at + 1..]);
            if !local.is_empty() && domain.contains('.') && !domain.starts_with('.') {
                out.push(t.to_lowercase());
            }
        }
    }
    out
}

pub fn extract_domain(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if let Some(start) = lower.find("://") {
        let rest = &lower[start + 3..];
        let host = rest.split(['/', '?', '#', ' ']).next().unwrap_or("");
        if !host.is_empty() {
            return Some(host.trim_end_matches('.').to_string());
        }
    }
    // Fall back to the first dotted token, which is how a window title
    // reveals a site when no URL was supplied.
    for token in lower.split(|c: char| c.is_whitespace() || "\"'<>(),;:[]|".contains(c)) {
        let t = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '.');
        if let Some(dot) = t.rfind('.') {
            let tld = &t[dot + 1..];
            if tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()) && dot > 0 {
                return Some(t.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

pub struct SafetyGate {
    config: GateConfig,
    limiter: RateLimiter,
    pub kill: KillSwitch,
}

impl SafetyGate {
    pub fn new(config: GateConfig) -> Self {
        let limiter = RateLimiter::new(config.rate_limit_per_minute);
        SafetyGate { config, limiter, kill: KillSwitch::default() }
    }

    pub fn config(&self) -> &GateConfig {
        &self.config
    }

    /// Swap the policy in without losing the kill state or the rate window --
    /// a config edit must not be a way to clear an engaged kill switch.
    pub fn reconfigure(&mut self, config: GateConfig) {
        self.limiter.set_limit(config.rate_limit_per_minute);
        self.config = config;
    }

    /// Classify without consuming rate-limit budget. Used for `dry_run`.
    pub fn classify(&self, instruction: &str, target: &TargetRef) -> Verdict {
        if let Some(scope) = self.kill.covers(target) {
            let mut v = Verdict::new(Category::Killed, Some(format!("kill_switch:{scope}")));
            v.scope_checked = Some(scope);
            return v;
        }

        let raw = instruction.to_lowercase();
        let reviewable = reviewable_text(instruction).to_lowercase();
        let title = target.window_title.to_lowercase();

        // Block keywords see the raw string, quoted payload included: typing
        // `rm -rf /` into a terminal is as destructive as clicking it.
        for kw in &self.config.block_keywords {
            if contains_phrase(&raw, kw) {
                return Verdict::new(Category::Block, Some(format!("block_keyword:\"{kw}\"")));
            }
        }

        let domain = target.domain();
        if let Some(d) = &domain {
            if let Some(hit) = self.config.deny_domains.iter().find(|x| domain_matches(d, x)) {
                return Verdict::new(Category::Block, Some(format!("deny_domain:{hit}")));
            }
        }
        if let Some(hit) = self
            .config
            .deny_apps
            .iter()
            .find(|a| contains_phrase(&title, a) || contains_phrase(&raw, a))
        {
            return Verdict::new(Category::Block, Some(format!("deny_app:{hit}")));
        }

        // Which browser, by brand. Not by window title: on a browser the
        // title is the page's, which is arbitrary text from the internet, and
        // gating on it means a page can decide whether it is allowed.
        if !self.config.allowed_browsers.is_empty() {
            if let Some(brand) = &target.brand {
                let brand = brand.to_ascii_lowercase();
                if !self
                    .config
                    .allowed_browsers
                    .iter()
                    .any(|b| b.trim().eq_ignore_ascii_case(&brand))
                {
                    return Verdict::new(
                        Category::Block,
                        Some(format!("browser_not_allowed:{brand}")),
                    );
                }
            }
        }

        if let Some(d) = &domain {
            if let Some(hit) = self.config.review_domains.iter().find(|x| domain_matches(d, x)) {
                return self.review(format!("review_domain:{hit}"), target);
            }
        }
        if let Some(hit) = self.config.review_apps.iter().find(|a| contains_phrase(&title, a)) {
            return self.review(format!("review_app:{hit}"), target);
        }

        // A send to someone not on the allowlist escalates on its own, even
        // with no keyword hit -- the allowlist is about *who*, and a keyword
        // list cannot express that.
        if !self.config.allow_recipients.is_empty() {
            let allow: Vec<String> =
                self.config.allow_recipients.iter().map(|r| r.to_lowercase()).collect();
            for addr in emails_in(instruction) {
                if !allow.iter().any(|a| recipient_matches(&addr, a)) {
                    return self.review(format!("recipient_not_allowed:{addr}"), target);
                }
            }
        }

        for kw in &self.config.review_keywords {
            if contains_phrase(&reviewable, kw) {
                return self.review(format!("review_keyword:\"{kw}\""), target);
            }
        }

        Verdict::allow()
    }

    /// Classify and consume one unit of rate-limit budget.
    pub fn check(&mut self, instruction: &str, target: &TargetRef) -> Verdict {
        // Kill first: a halted system must not spend budget, and must answer
        // the same way however many times it is asked.
        if let Some(scope) = self.kill.covers(target) {
            let mut v = Verdict::new(Category::Killed, Some(format!("kill_switch:{scope}")));
            v.scope_checked = Some(scope);
            return v;
        }
        if !self.limiter.allow() {
            return Verdict::new(Category::RateLimited, Some("rate_limit_exceeded".into()));
        }
        self.classify(instruction, target)
    }

    fn review(&self, rule: String, target: &TargetRef) -> Verdict {
        let prefix = rule.split(':').next().unwrap_or("").to_string();
        let requires_os_dialog = self
            .config
            .require_os_dialog_for
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&prefix) || rule.starts_with(r.as_str()));
        Verdict {
            category: Category::Review,
            rule: Some(rule),
            requires_os_dialog,
            scope_checked: target.brand.as_ref().map(|b| format!("browser:{b}")),
        }
    }

    pub fn review_dialog_timeout(&self) -> Duration {
        Duration::from_secs(self.config.review_dialog_timeout_s.max(1))
    }
}

/// `mail.google.com` matches a `google.com` rule; `notgoogle.com` does not.
fn domain_matches(domain: &str, rule: &str) -> bool {
    let d = domain.trim_start_matches('.');
    let r = rule.trim_start_matches('.');
    d == r || d.ends_with(&format!(".{r}"))
}

/// An allowlist entry is either a full address or a bare domain.
fn recipient_matches(addr: &str, rule: &str) -> bool {
    if rule.contains('@') {
        return addr == rule;
    }
    match addr.split_once('@') {
        Some((_, dom)) => domain_matches(dom, rule),
        None => false,
    }
}

/// What goes in the journal for typed text. The value never does when the
/// control was flagged sensitive, whatever the gate decided about the action.
pub fn redact_for_log(instruction: &str, sensitive: bool) -> String {
    if sensitive {
        "[REDACTED]".to_string()
    } else {
        instruction.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> SafetyGate {
        SafetyGate::new(GateConfig::default())
    }

    fn browser(title: &str, url: &str) -> TargetRef {
        TargetRef {
            window_title: title.into(),
            url: Some(url.into()),
            exe: Some("brave.exe".into()),
            brand: Some("brave".into()),
        }
    }

    // -- the bug that cost 120 seconds live ---------------------------------

    #[test]
    fn a_quoted_subject_line_containing_order_is_not_a_review() {
        let g = gate();
        let v = g.classify(
            "type \"Order-taking AI agents for distributors\" into the Subject field",
            &browser("Gmail", "https://mail.google.com/"),
        );
        assert_eq!(v.category, Category::Allow, "{:?}", v.rule);
    }

    #[test]
    fn but_clicking_send_still_reviews() {
        let g = gate();
        let v = g.classify("click the \"Send\" button", &browser("Gmail", "https://mail.google.com/"));
        assert_eq!(v.category, Category::Review);
        assert_eq!(v.rule.as_deref(), Some("review_keyword:\"send\""));
    }

    #[test]
    fn typing_a_destructive_command_is_still_blocked() {
        // Block keywords deliberately see the quoted payload.
        let g = gate();
        let v = g.classify("type \"rm -rf /\" into the terminal", &TargetRef::default());
        assert_eq!(v.category, Category::Block);
    }

    #[test]
    fn an_apostrophe_does_not_swallow_a_keyword() {
        // The hole in the Python version: 'don't ... Bob's' bracketed a span
        // and removed everything between the two apostrophes.
        let g = gate();
        let v = g.classify(
            "type the note but don't delete Bob's draft",
            &TargetRef::default(),
        );
        assert_eq!(v.category, Category::Review, "delete must survive the contraction");
        assert_eq!(v.rule.as_deref(), Some("review_keyword:\"delete\""));
    }

    #[test]
    fn unbalanced_quote_does_not_swallow_the_rest() {
        let g = gate();
        let v = g.classify("type \"hello and then delete it", &TargetRef::default());
        assert_eq!(v.category, Category::Review);
    }

    // -- word boundaries ----------------------------------------------------

    #[test]
    fn keywords_match_words_not_substrings() {
        let g = gate();
        // "sender" must not trip "send"
        assert_eq!(
            g.classify("click the sender column header", &TargetRef::default()).category,
            Category::Allow
        );
        assert_eq!(
            g.classify("click the \"Send\" button", &TargetRef::default()).category,
            Category::Review
        );
        // multi-word keyword, contiguous
        assert_eq!(
            g.classify("click \"cancel subscription\"", &TargetRef::default()).category,
            Category::Review
        );
        assert_eq!(
            g.classify("cancel the subscription dialog is open", &TargetRef::default()).category,
            Category::Allow
        );
    }

    // -- domains ------------------------------------------------------------

    #[test]
    fn review_domain_matches_subdomains_only() {
        let g = gate();
        assert_eq!(
            g.classify("click continue", &browser("Chase", "https://secure.chase.com/pay")).category,
            Category::Review
        );
        assert_eq!(
            g.classify("click continue", &browser("x", "https://notchase.com/")).category,
            Category::Allow
        );
    }

    #[test]
    fn domain_extraction_prefers_the_url() {
        assert_eq!(extract_domain("https://mail.google.com/u/0"), Some("mail.google.com".into()));
        assert_eq!(extract_domain("Inbox - Gmail"), None);
        assert_eq!(extract_domain("open paypal.com now"), Some("paypal.com".into()));
    }

    // -- recipient allowlist ------------------------------------------------

    #[test]
    fn a_recipient_off_the_allowlist_escalates_without_any_keyword() {
        let mut cfg = GateConfig::default();
        cfg.review_keywords.clear();
        cfg.allow_recipients = vec!["aditya03y3@gmail.com".into(), "example.org".into()];
        let g = SafetyGate::new(cfg);

        let v = g.classify("put stranger@elsewhere.com in the To field", &TargetRef::default());
        assert_eq!(v.category, Category::Review);
        assert!(v.rule.as_deref().unwrap().starts_with("recipient_not_allowed:"));
        assert!(v.requires_os_dialog, "an unlisted recipient must need a real click");

        // exact address, and a whole allowed domain
        assert_eq!(
            g.classify("type \"aditya03y3@gmail.com\" in To", &TargetRef::default()).category,
            Category::Allow
        );
        assert_eq!(
            g.classify("type \"anyone@example.org\" in To", &TargetRef::default()).category,
            Category::Allow
        );
    }

    #[test]
    fn an_empty_allowlist_does_not_gate_recipients() {
        let mut cfg = GateConfig::default();
        cfg.review_keywords.clear();
        let g = SafetyGate::new(cfg);
        assert_eq!(
            g.classify("click the row for stranger@elsewhere.com", &TargetRef::default()).category,
            Category::Allow
        );
    }

    #[test]
    fn email_scanner_finds_addresses_in_punctuation() {
        assert_eq!(emails_in("to: <a.b@x.co>, c@y.org;"), vec!["a.b@x.co", "c@y.org"]);
        assert!(emails_in("no address here @ all").is_empty());
        assert!(emails_in("a@nodot").is_empty());
    }

    // -- which browser ------------------------------------------------------

    #[test]
    fn only_the_listed_browsers_may_be_driven() {
        let mut cfg = GateConfig::default();
        cfg.review_keywords.clear();
        cfg.allowed_browsers = vec!["brave".into()];
        let g = SafetyGate::new(cfg);

        let brave = browser("Anything - Brave", "https://example.com/");
        assert_eq!(g.classify("click ok", &brave).category, Category::Allow);

        let chrome = TargetRef {
            window_title: "Anything".into(),
            url: Some("https://example.com/".into()),
            exe: Some("chrome.exe".into()),
            brand: Some("chrome".into()),
        };
        let v = g.classify("click ok", &chrome);
        assert_eq!(v.category, Category::Block);
        assert_eq!(v.rule.as_deref(), Some("browser_not_allowed:chrome"));

        // A native window is not a browser and is unaffected by the rule.
        let notepad = TargetRef { window_title: "Untitled - Notepad".into(), ..Default::default() };
        assert_eq!(g.classify("click ok", &notepad).category, Category::Allow);
    }

    #[test]
    fn a_page_title_cannot_decide_which_browser_is_allowed() {
        // The mechanism this replaced matched window titles, so a page called
        // "Chrome vs Brave" blocked itself -- and, worse, a page could have
        // named itself into or out of the rule.
        let mut cfg = GateConfig::default();
        cfg.review_keywords.clear();
        cfg.allowed_browsers = vec!["brave".into()];
        let g = SafetyGate::new(cfg);

        let hostile = TargetRef {
            window_title: "Google Chrome vs Microsoft Edge - YouTube - Brave".into(),
            url: Some("https://youtube.com/watch".into()),
            exe: Some("brave.exe".into()),
            brand: Some("brave".into()),
        };
        assert_eq!(g.classify("click play", &hostile).category, Category::Allow);
    }

    #[test]
    fn an_empty_browser_allowlist_permits_every_browser() {
        let mut cfg = GateConfig::default();
        cfg.review_keywords.clear();
        let g = SafetyGate::new(cfg);
        let chrome = TargetRef {
            window_title: "x".into(),
            exe: Some("chrome.exe".into()),
            brand: Some("chrome".into()),
            ..Default::default()
        };
        assert_eq!(g.classify("click ok", &chrome).category, Category::Allow);
    }

    // -- kill switch --------------------------------------------------------

    #[test]
    fn a_scoped_kill_stops_one_browser_and_nothing_else() {
        let mut g = gate();
        g.kill.engage("browser:brave");

        let brave = browser("Gmail", "https://mail.google.com/");
        let chrome = TargetRef {
            window_title: "Docs".into(),
            url: None,
            exe: Some("chrome.exe".into()),
            brand: Some("chrome".into()),
        };
        let notepad = TargetRef { window_title: "Untitled - Notepad".into(), ..Default::default() };

        assert_eq!(g.classify("click anything", &brave).category, Category::Killed);
        assert_eq!(g.classify("click anything", &chrome).category, Category::Allow);
        assert_eq!(g.classify("click anything", &notepad).category, Category::Allow);
    }

    #[test]
    fn global_kill_stops_everything_and_reset_clears_all_scopes() {
        let mut g = gate();
        g.kill.engage("browser:brave");
        g.kill.engage("global");
        assert_eq!(g.classify("click x", &TargetRef::default()).category, Category::Killed);
        g.kill.reset("global");
        assert!(!g.kill.is_engaged(), "clearing global means resume, not resume-except-brave");
        assert_eq!(g.classify("click x", &browser("a", "https://b.com")).category, Category::Allow);
    }

    #[test]
    fn app_scope_matches_the_executable() {
        let mut g = gate();
        g.kill.engage("app:excel.exe");
        let excel = TargetRef {
            window_title: "Book1 - Excel".into(),
            exe: Some("EXCEL.EXE".into()),
            ..Default::default()
        };
        assert_eq!(g.classify("click A1", &excel).category, Category::Killed);
    }

    #[test]
    fn kill_survives_a_reconfigure() {
        // A config edit must not be a back door for clearing the kill switch.
        let mut g = gate();
        g.kill.engage("global");
        g.reconfigure(GateConfig::default());
        assert!(g.kill.is_engaged());
    }

    #[test]
    fn killed_targets_do_not_consume_rate_budget() {
        let cfg = GateConfig { rate_limit_per_minute: 2, ..Default::default() };
        let mut g = SafetyGate::new(cfg);
        g.kill.engage("global");
        for _ in 0..10 {
            assert_eq!(g.check("click x", &TargetRef::default()).category, Category::Killed);
        }
        g.kill.reset_all();
        // Budget untouched by the ten refusals.
        assert_ne!(g.check("click x", &TargetRef::default()).category, Category::RateLimited);
    }

    // -- rate limit ---------------------------------------------------------

    #[test]
    fn rate_limit_trips_then_classifies_again_after_the_window() {
        let cfg = GateConfig { rate_limit_per_minute: 3, ..Default::default() };
        let mut g = SafetyGate::new(cfg);
        for _ in 0..3 {
            assert_ne!(g.check("click ok", &TargetRef::default()).category, Category::RateLimited);
        }
        assert_eq!(g.check("click ok", &TargetRef::default()).category, Category::RateLimited);

        // Rewind the window rather than sleeping 60s in a unit test.
        let past = Instant::now() - Duration::from_secs(61);
        g.limiter.hits = vec![past; 3];
        assert_ne!(g.check("click ok", &TargetRef::default()).category, Category::RateLimited);
    }

    // -- os dialog escalation ----------------------------------------------

    #[test]
    fn a_bank_domain_requires_a_real_click_but_a_keyword_does_not() {
        let g = gate();
        let bank = g.classify("click continue", &browser("Chase", "https://chase.com/transfer"));
        assert!(bank.requires_os_dialog);

        let keyword = g.classify("click the \"Send\" button", &browser("Gmail", "https://mail.google.com"));
        assert_eq!(keyword.category, Category::Review);
        assert!(!keyword.requires_os_dialog, "ordinary review is agent-confirmable");
    }

    // -- redaction ----------------------------------------------------------

    #[test]
    fn sensitive_values_never_reach_the_log() {
        assert_eq!(redact_for_log("hunter2", true), "[REDACTED]");
        assert_eq!(redact_for_log("hello", false), "hello");
    }

    #[test]
    fn verdict_report_round_trips_to_the_wire_shape() {
        let g = gate();
        let v = g.classify("click the \"Send\" button", &browser("Gmail", "https://mail.google.com"));
        let r = v.report();
        assert_eq!(r.category, "review");
        assert_eq!(r.rule.as_deref(), Some("review_keyword:\"send\""));
        assert_eq!(r.scope_checked.as_deref(), Some("browser:brave"));
    }
}
