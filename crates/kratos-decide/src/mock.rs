//! A deterministic offline provider.
//!
//! Not a toy: it is what makes the broker, the gate, the lease, the journal
//! and the whole effect-verification path testable without a network, a key,
//! or a bill. Requirement 8's two-agents-two-browsers test runs entirely on
//! this.
//!
//! It scores each option by word overlap with the state text and converts the
//! peak probability into a confidence the same way a real distribution would,
//! so the confidence *shape* is realistic even though the reasoning is not.
//! That matters: a mock that always answered 1.0 would let a broken threshold
//! pass every test.
//!
//! It is explicitly **not** a substitute for measuring thresholds against the
//! real model.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::{Answers, ChoiceAnswer, DecideError, DecisionProvider, Question};

pub struct MockProvider {
    model: String,
    /// Forces the next answer's confidence, for tests that need a refusal.
    pub forced_confidence: Option<f64>,
    /// Forces a specific choice per question name.
    pub forced_choice: BTreeMap<String, String>,
}

impl MockProvider {
    pub fn new(model: String) -> Self {
        MockProvider { model, forced_confidence: None, forced_choice: BTreeMap::new() }
    }

    pub fn with_confidence(mut self, c: f64) -> Self {
        self.forced_confidence = Some(c);
        self
    }

    pub fn choosing(mut self, question: &str, option: &str) -> Self {
        self.forced_choice.insert(question.to_string(), option.to_string());
        self
    }
}

/// The state's *values*, not its structure.
///
/// Stringifying the whole JSON object put field names into the haystack, so a
/// state carrying `instruction` as a key scored a match for an option whose
/// description contains that word -- which "none of these" does.
fn state_text(state: &Value) -> String {
    let mut out = String::new();
    collect_values(state, &mut out);
    out.to_lowercase()
}

fn collect_values(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Number(n) => {
            out.push_str(&n.to_string());
            out.push(' ');
        }
        Value::Array(a) => {
            for x in a {
                collect_values(x, out);
            }
        }
        // Keys are deliberately skipped; only what they hold is evidence.
        Value::Object(o) => {
            for x in o.values() {
                collect_values(x, out);
            }
        }
        _ => {}
    }
}

/// Words worth matching on: long enough to mean something, and not a
/// function word every criterion shares. Without this filter a single "the"
/// in common could decide an answer.
fn significant(s: &str) -> impl Iterator<Item = String> + '_ {
    const STOP: [&str; 22] = [
        "the", "and", "any", "are", "for", "from", "into", "not", "that", "this", "with", "was",
        "can", "its", "has", "all", "one", "out", "use", "been", "have", "does",
    ];
    s.to_lowercase()
        .replace('_', " ")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOP.contains(w))
        .map(|w| w.to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

impl DecisionProvider for MockProvider {
    fn name(&self) -> String {
        format!("mock/{}", self.model)
    }

    fn configured(&self) -> bool {
        true
    }

    fn ask(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, DecideError> {
        if questions.is_empty() {
            return Err(DecideError::Malformed("no questions asked".into()));
        }
        let text = state_text(state);
        let mut out = Answers {
            model: format!("{}-mock", self.model),
            input_tokens: (text.len() / 4).max(1) as u64,
            latency_ms: 3,
            ..Default::default()
        };

        for (name, q) in questions {
            match q {
                Question::Choice { criteria, .. } => {
                    if criteria.is_empty() {
                        return Err(DecideError::Malformed(format!("{name:?} has no options")));
                    }
                    // The question's own prompt is NOT part of the haystack.
                    // It once was, and since `decide` asks "which action kind
                    // does this instruction call for", the words "instruction"
                    // and "action" scored for the one criterion that contains
                    // them -- "the instruction does not describe any action
                    // that can be taken" -- so every call leaned to `none`.
                    // A prompt is the question, not the evidence.
                    let scored: Vec<(String, f64)> = criteria
                        .iter()
                        .map(|(opt, desc)| {
                            let hits = significant(&format!("{opt} {desc}"))
                                .filter(|w| text.contains(w.as_str()))
                                .count();
                            (opt.clone(), hits as f64)
                        })
                        .collect();

                    let total: f64 = scored.iter().map(|(_, v)| v).sum::<f64>()
                        + scored.len() as f64 * 0.25;
                    let probabilities: BTreeMap<String, f64> = scored
                        .iter()
                        .map(|(k, v)| (k.clone(), ((v + 0.25) / total * 10_000.0).round() / 10_000.0))
                        .collect();

                    let best = self.forced_choice.get(name).cloned().unwrap_or_else(|| {
                        scored
                            .iter()
                            .enumerate()
                            // Ties go to the option listed first, not last.
                            // `none` is last in the kind list, so the previous
                            // tie-break handed every ambiguous call to "no
                            // action possible".
                            .max_by(|(ia, a), (ib, b)| {
                                a.1.partial_cmp(&b.1)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                                    .then(ib.cmp(ia))
                            })
                            .map(|(_, (k, _))| k.clone())
                            .unwrap_or_default()
                    });

                    // Peak-to-uniform confidence: 1.0 when all mass is on one
                    // option, 0.0 when the distribution is flat. The same
                    // shape a calibrated provider produces, so a test that
                    // relies on refusal actually exercises refusal.
                    let n = probabilities.len().max(1) as f64;
                    let peak = probabilities.get(&best).copied().unwrap_or(0.0);
                    let confidence = self.forced_confidence.unwrap_or(if n > 1.0 {
                        ((n * peak - 1.0) / (n - 1.0)).clamp(0.0, 1.0)
                    } else {
                        1.0
                    });

                    out.choices
                        .insert(name.clone(), ChoiceAnswer { choice: best, probabilities, confidence });
                }
                Question::Noul { instructions } => {
                    const SIGNALS: [&str; 8] = [
                        "delete", "remove", "send", "pay", "wipe", "destroy", "cancel", "uninstall",
                    ];
                    let looks_destructive =
                        SIGNALS.iter().any(|w| text.contains(w) || instructions.contains(w));
                    out.nouls
                        .insert(name.clone(), if looks_destructive { 0.82 } else { 0.12 });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind_question() -> BTreeMap<String, Question> {
        let mut q = BTreeMap::new();
        q.insert(
            "kind".to_string(),
            Question::Choice {
                instructions: "which action kind".into(),
                criteria: vec![
                    ("click".into(), "press a button or link once with the mouse".into()),
                    ("navigate".into(), "go directly to a url or website address".into()),
                    ("type".into(), "enter literal characters into a text field".into()),
                ],
            },
        );
        q
    }

    #[test]
    fn the_questions_own_prompt_is_not_evidence() {
        // Verbatim the prompt `decide` sends. It contains "instruction" and
        // "action", which are the distinctive words of the `none` criterion.
        let mut q = BTreeMap::new();
        q.insert(
            "kind".to_string(),
            Question::Choice {
                instructions:
                    "Which single action kind does this instruction call for, on the window titled \"Gmail\""
                        .into(),
                criteria: vec![
                    ("click".into(), "Press a button, link, menu item or checkbox once with the mouse".into()),
                    (
                        "none".into(),
                        "The instruction does not describe any action that can be taken on this screen"
                            .into(),
                    ),
                ],
            },
        );
        let a = MockProvider::new("m".into())
            .ask(&json!({"instruction": "click the Inbox link"}), &q)
            .unwrap();
        assert_eq!(a.choice("kind").unwrap().choice, "click");
    }

    #[test]
    fn field_names_in_the_state_are_not_evidence() {
        // A state object carrying the word "instruction" as a *key* must not
        // score a hit for an option whose description contains that word.
        let p = MockProvider::new("m".into());
        let state = json!({"instruction": "click the Inbox link", "window": "Gmail"});
        let a = p.ask(&state, &kind_question()).unwrap();
        assert_eq!(
            a.choice("kind").unwrap().choice,
            "click",
            "a plain click must not come back as something else"
        );
    }

    #[test]
    fn ties_go_to_the_first_option_not_the_last() {
        let mut q = BTreeMap::new();
        q.insert(
            "kind".to_string(),
            Question::Choice {
                instructions: "which".into(),
                criteria: vec![
                    ("click".into(), "zzz".into()),
                    ("none".into(), "zzz".into()),
                ],
            },
        );
        let a = MockProvider::new("m".into()).ask(&json!("nothing matches"), &q).unwrap();
        assert_eq!(a.choice("kind").unwrap().choice, "click");
    }

    #[test]
    fn it_picks_the_overlapping_option() {
        let p = MockProvider::new("m".into());
        let a = p
            .ask(&json!("navigate to a website address in the browser"), &kind_question())
            .unwrap();
        assert_eq!(a.choice("kind").unwrap().choice, "navigate");
    }

    #[test]
    fn confidence_falls_when_nothing_matches() {
        let p = MockProvider::new("m".into());
        let a = p.ask(&json!("xyzzy plugh frotz"), &kind_question()).unwrap();
        let c = a.choice("kind").unwrap().confidence;
        // A flat distribution must not read as certainty, or a test relying
        // on refusal would silently stop testing it.
        assert!(c < 0.2, "flat distribution gave confidence {c}");
    }

    #[test]
    fn probabilities_sum_to_about_one() {
        let p = MockProvider::new("m".into());
        let a = p.ask(&json!("click the button"), &kind_question()).unwrap();
        let sum: f64 = a.choice("kind").unwrap().probabilities.values().sum();
        assert!((sum - 1.0).abs() < 0.01, "{sum}");
    }

    #[test]
    fn it_is_deterministic() {
        let p = MockProvider::new("m".into());
        let one = p.ask(&json!("click send"), &kind_question()).unwrap();
        let two = p.ask(&json!("click send"), &kind_question()).unwrap();
        assert_eq!(one.choice("kind").unwrap().choice, two.choice("kind").unwrap().choice);
        assert_eq!(
            one.choice("kind").unwrap().confidence,
            two.choice("kind").unwrap().confidence
        );
    }

    #[test]
    fn forcing_lets_a_test_drive_refusal_and_wrong_answers() {
        let p = MockProvider::new("m".into()).with_confidence(0.42).choosing("kind", "type");
        let a = p.ask(&json!("click the button"), &kind_question()).unwrap();
        assert_eq!(a.choice("kind").unwrap().choice, "type");
        assert_eq!(a.weakest(&["kind"]), 0.42);
    }

    #[test]
    fn an_option_list_with_no_entries_is_an_error() {
        let mut q = BTreeMap::new();
        q.insert(
            "target".to_string(),
            Question::Choice { instructions: "which".into(), criteria: vec![] },
        );
        assert!(matches!(
            MockProvider::new("m".into()).ask(&json!("x"), &q),
            Err(DecideError::Malformed(_))
        ));
    }

    #[test]
    fn nouls_answer_as_a_probability() {
        let mut q = BTreeMap::new();
        q.insert(
            "destructive".to_string(),
            Question::Noul { instructions: "is this hard to undo".into() },
        );
        let p = MockProvider::new("m".into());
        assert!(p.ask(&json!("delete the file"), &q).unwrap().noul("destructive").unwrap() > 0.5);
        assert!(p.ask(&json!("read the title"), &q).unwrap().noul("destructive").unwrap() < 0.5);
    }

    #[test]
    fn a_mock_is_always_configured_so_tests_need_no_key() {
        assert!(MockProvider::new("m".into()).configured());
        assert_eq!(MockProvider::new("m".into()).name(), "mock/m");
    }
}
