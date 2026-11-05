//! The decision layer: which action kind, and which element.
//!
//! This is the only place in kratos that talks to a model, and the only
//! place that holds the API key. Backends never see it; the MCP shim never
//! sees it.
//!
//! The model decides **the action and the target, and nothing else**. Every
//! literal -- the characters to type, the key to press, the URL to open -- is
//! extracted from the operator's own instruction string by `extract`. A model
//! that generates no text cannot invent a recipient, and that is a property
//! worth keeping rather than an inconvenience to work around.
//!
//! ## Why the provider is a trait
//!
//! The wire format here is System One: `{state, questions:{name:{type,
//! instructions, criteria}}}` with Bearer auth, answers carrying a label, a
//! probability distribution and a calibrated confidence. TypeSafe's Jev speaks
//! it; so does Cloudflare's open-weight Clef family, which differs only in the
//! endpoint URL shape. Swapping provider is therefore a config change.
//!
//! The one hard requirement on a replacement is the **distribution**. A
//! provider that returns only a label cannot be used, because then the
//! per-kind thresholds have nothing to threshold and every answer looks
//! equally certain -- which is the failure this whole design exists to avoid.

pub mod candidates;
pub mod extract;
pub mod jev;
pub mod mock;

use std::collections::BTreeMap;

pub use candidates::{compress, Candidate, NONE_OF_THESE};

/// Jev's documented ceiling on options in one choice.
pub const MAX_CARDINALITY: usize = 255;

#[derive(Debug, Clone)]
pub enum Question {
    /// Pick one option. `criteria` is ordered `(name, description)`.
    Choice { instructions: String, criteria: Vec<(String, String)> },
    /// Yes/no as a probability. The number *is* the belief; there is no
    /// separate confidence.
    Noul { instructions: String },
}

#[derive(Debug, Clone, Default)]
pub struct ChoiceAnswer {
    pub choice: String,
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Answers {
    pub model: String,
    pub choices: BTreeMap<String, ChoiceAnswer>,
    pub nouls: BTreeMap<String, f64>,
    pub input_tokens: u64,
    pub latency_ms: u64,
}

impl Answers {
    pub fn choice(&self, name: &str) -> Option<&ChoiceAnswer> {
        self.choices.get(name)
    }

    pub fn noul(&self, name: &str) -> Option<f64> {
        self.nouls.get(name).copied()
    }

    /// The weakest confidence across whichever of `names` were answered.
    ///
    /// The threshold is applied to this, not to an average: an action the
    /// model is certain about, aimed at an element it is guessing at, is
    /// exactly the failure that typed a subject line into a To field. The
    /// weakest link is the one that decides.
    ///
    /// Names that were never asked are skipped -- `navigate` carries no target
    /// question -- but if *none* of them were answered the result is 0.0, not
    /// 1.0. An empty answer set means the provider told us nothing, and
    /// "nothing" must fail every threshold rather than clear all of them.
    pub fn weakest(&self, names: &[&str]) -> f64 {
        let mut min = f64::INFINITY;
        for name in names {
            if let Some(c) = self.choices.get(*name) {
                min = min.min(c.confidence);
            }
        }
        if min.is_finite() {
            min.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

#[derive(Debug)]
pub enum DecideError {
    /// No API key. Distinct from a transport failure because the remedy is
    /// configuration, not a retry.
    Unconfigured,
    Http { status: u16, body: String },
    Transport(String),
    /// A reply this build cannot read. Reported rather than defaulted: a
    /// confidence silently read as 0.0 would look like a refusal, and one read
    /// as 1.0 would be catastrophic.
    Malformed(String),
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecideError::Unconfigured => write!(f, "no decider API key configured"),
            DecideError::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            DecideError::Transport(m) => write!(f, "transport: {m}"),
            DecideError::Malformed(m) => write!(f, "unreadable reply: {m}"),
        }
    }
}

impl std::error::Error for DecideError {}

impl DecideError {
    /// The wire error code this maps to.
    pub fn code(&self) -> &'static str {
        match self {
            DecideError::Unconfigured => kratos_protocol::codes::DECIDER_UNCONFIGURED,
            _ => kratos_protocol::codes::DECIDER_ERROR,
        }
    }
}

/// A source of typed decisions.
///
/// Blocking on purpose: the implementations are HTTP round trips of 70-500ms,
/// and the broker calls them from a blocking pool. Making the trait async
/// would force every implementor to pick a runtime for no benefit.
pub trait DecisionProvider: Send + Sync {
    /// `provider/model`, as it appears in the journal and in `status`.
    fn name(&self) -> String;

    /// Whether a call can be attempted at all.
    fn configured(&self) -> bool;

    fn ask(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, DecideError>;
}

/// Build the configured provider.
///
/// `api_key` arrives already unsealed by the caller -- this crate never reads
/// the config file and never touches DPAPI, so the key exists in exactly one
/// process and one call path.
pub fn build(
    cfg: &kratos_protocol::config::Decider,
    api_key: String,
) -> std::sync::Arc<dyn DecisionProvider> {
    match cfg.provider.trim().to_ascii_lowercase().as_str() {
        "mock" => std::sync::Arc::new(mock::MockProvider::new(cfg.model.clone())),
        _ => std::sync::Arc::new(jev::SystemOneProvider::new(cfg.clone(), api_key)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weakest_picks_the_minimum_not_the_mean() {
        let mut a = Answers::default();
        a.choices.insert(
            "kind".into(),
            ChoiceAnswer { choice: "click".into(), confidence: 0.99, ..Default::default() },
        );
        a.choices.insert(
            "target".into(),
            ChoiceAnswer { choice: "e4".into(), confidence: 0.61, ..Default::default() },
        );
        // The mean would be 0.80 and would clear a 0.70 bar. The minimum does
        // not, which is the correct answer: the target is a guess.
        assert_eq!(a.weakest(&["kind", "target"]), 0.61);
    }

    #[test]
    fn weakest_of_one_question_is_that_question() {
        let mut a = Answers::default();
        a.choices.insert(
            "kind".into(),
            ChoiceAnswer { choice: "navigate".into(), confidence: 0.88, ..Default::default() },
        );
        assert_eq!(a.weakest(&["kind", "target"]), 0.88);
    }

    #[test]
    fn weakest_of_nothing_refuses_rather_than_passing_everything() {
        // The dangerous default: folding over an empty set with min() yields
        // infinity, and clamping that to 1.0 would make a provider that
        // answered nothing look maximally certain.
        assert_eq!(Answers::default().weakest(&["kind", "target"]), 0.0);
    }

    #[test]
    fn confidence_outside_zero_to_one_is_clamped() {
        let mut a = Answers::default();
        a.choices.insert(
            "kind".into(),
            ChoiceAnswer { choice: "click".into(), confidence: 1.7, ..Default::default() },
        );
        assert_eq!(a.weakest(&["kind"]), 1.0);
        a.choices.insert(
            "kind".into(),
            ChoiceAnswer { choice: "click".into(), confidence: -3.0, ..Default::default() },
        );
        assert_eq!(a.weakest(&["kind"]), 0.0);
    }

    #[test]
    fn unconfigured_maps_to_its_own_wire_code() {
        assert_eq!(DecideError::Unconfigured.code(), "decider_unconfigured");
        assert_eq!(DecideError::Transport("x".into()).code(), "decider_error");
    }
}
