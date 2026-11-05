//! The System One provider: Jev, Clef, or anything else speaking that format.
//!
//! Wire shape, which is the same for every provider here:
//!
//! ```text
//! POST <endpoint>
//! Authorization: Bearer <key>
//! {"model": "...", "state": <any JSON>, "questions": {
//!    "<name>": {"type": "choice", "instructions": "...", "criteria": {...}}
//! }}
//! ```
//!
//! and back, per question, a label with a probability distribution and a
//! calibrated confidence. Only the endpoint URL differs between providers:
//! TypeSafe exposes one path for every model, Cloudflare names the model in
//! the path. Both are just `decider.endpoint` in the config, so switching is a
//! settings change.
//!
//! Parsing is deliberately tolerant of two key spellings per field. The
//! alternative -- one rename upstream taking the whole tool down -- is worse
//! than carrying a fallback. What it is *not* tolerant of is a missing
//! confidence: that is reported as malformed rather than defaulted, because a
//! confidence guessed at 0.0 reads as a refusal and one guessed at 1.0 would
//! execute on no evidence at all.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{Answers, ChoiceAnswer, DecideError, DecisionProvider, Question};

pub struct SystemOneProvider {
    cfg: kratos_protocol::config::Decider,
    api_key: String,
    agent: ureq::Agent,
}

impl SystemOneProvider {
    pub fn new(cfg: kratos_protocol::config::Decider, api_key: String) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(cfg.timeout_ms.max(500))))
            // Read the body on a 4xx: the provider's own message is far more
            // useful to a human than "HTTP 401".
            .http_status_as_error(false)
            .build()
            .into();
        SystemOneProvider { cfg, api_key, agent }
    }

    fn request_body(&self, state: &Value, questions: &BTreeMap<String, Question>) -> Value {
        let mut qs = serde_json::Map::new();
        for (name, q) in questions {
            let encoded = match q {
                Question::Choice { instructions, criteria } => {
                    let mut c = serde_json::Map::new();
                    for (k, v) in criteria {
                        c.insert(k.clone(), Value::String(v.clone()));
                    }
                    json!({"type": "choice", "instructions": instructions, "criteria": c})
                }
                Question::Noul { instructions } => {
                    json!({"type": "noul", "instructions": instructions})
                }
            };
            qs.insert(name.clone(), encoded);
        }
        json!({"model": self.cfg.model, "state": state, "questions": qs})
    }
}

impl DecisionProvider for SystemOneProvider {
    fn name(&self) -> String {
        format!("{}/{}", self.cfg.provider, self.cfg.model)
    }

    fn configured(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    fn ask(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, DecideError> {
        if !self.configured() {
            return Err(DecideError::Unconfigured);
        }
        if questions.is_empty() {
            return Err(DecideError::Malformed("no questions asked".into()));
        }

        let body = self.request_body(state, questions);
        let started = Instant::now();
        let response = self
            .agent
            .post(&self.cfg.endpoint)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|e| DecideError::Transport(e.to_string()))?;

        let status = response.status().as_u16();
        let text = response
            .into_body()
            .read_to_string()
            .map_err(|e| DecideError::Transport(e.to_string()))?;
        let latency_ms = started.elapsed().as_millis() as u64;

        if status >= 400 {
            // Truncated: a provider error page can be a megabyte of HTML, and
            // this string goes into a log a human reads.
            let mut body = text;
            body.truncate(800);
            return Err(DecideError::Http { status, body });
        }

        let data: Value = serde_json::from_str(&text)
            .map_err(|e| DecideError::Malformed(format!("{e} in {}", preview(&text))))?;
        parse_response(&data, questions, &self.cfg.model, latency_ms)
    }
}

fn preview(s: &str) -> String {
    let mut p: String = s.chars().take(200).collect();
    if s.len() > 200 {
        p.push('\u{2026}');
    }
    p
}

/// Map a System One reply onto typed answers.
pub fn parse_response(
    data: &Value,
    questions: &BTreeMap<String, Question>,
    fallback_model: &str,
    latency_ms: u64,
) -> Result<Answers, DecideError> {
    // Answers may be nested under "answers" or sit at the top level beside
    // "model" and "usage".
    let answers = data.get("answers").filter(|v| v.is_object()).unwrap_or(data);

    let mut out = Answers {
        model: data
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(fallback_model)
            .to_string(),
        input_tokens: data
            .get("usage")
            .and_then(|u| u.get("input_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0),
        latency_ms,
        ..Default::default()
    };

    for (name, q) in questions {
        let a = answers
            .get(name)
            .ok_or_else(|| DecideError::Malformed(format!("no answer for {name:?}")))?;
        match q {
            Question::Choice { .. } => {
                let choice = a
                    .get("choice")
                    .or_else(|| a.get("value"))
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| {
                        DecideError::Malformed(format!("{name:?} has no choice label"))
                    })?
                    .to_string();
                let confidence = a
                    .get("confidence")
                    .and_then(|c| c.as_f64())
                    // Not defaulted: a provider that returns a label without a
                    // distribution cannot be gated on, and silently assuming a
                    // number would hide exactly that.
                    .ok_or_else(|| {
                        DecideError::Malformed(format!(
                            "{name:?} has no confidence -- this provider returns labels without a \
                             calibrated distribution and cannot be used for gated actions"
                        ))
                    })?;
                let mut probabilities = BTreeMap::new();
                if let Some(p) = a.get("probabilities").and_then(|p| p.as_object()) {
                    for (k, v) in p {
                        if let Some(f) = v.as_f64() {
                            probabilities.insert(k.clone(), f);
                        }
                    }
                }
                out.choices.insert(
                    name.clone(),
                    ChoiceAnswer { choice, probabilities, confidence: confidence.clamp(0.0, 1.0) },
                );
            }
            Question::Noul { .. } => {
                let v = a
                    .get("noul")
                    .or_else(|| a.get("value"))
                    .and_then(|c| c.as_f64())
                    .ok_or_else(|| DecideError::Malformed(format!("{name:?} has no noul")))?;
                out.nouls.insert(name.clone(), v.clamp(0.0, 1.0));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decider() -> kratos_protocol::config::Decider {
        kratos_protocol::config::Decider::default()
    }

    fn two_questions() -> BTreeMap<String, Question> {
        let mut q = BTreeMap::new();
        q.insert(
            "kind".to_string(),
            Question::Choice {
                instructions: "which action".into(),
                criteria: vec![("click".into(), "press a button".into())],
            },
        );
        q.insert(
            "target".to_string(),
            Question::Choice {
                instructions: "which element".into(),
                criteria: vec![("e1".into(), "button \"Send\"".into())],
            },
        );
        q
    }

    #[test]
    fn request_body_matches_the_system_one_shape() {
        let p = SystemOneProvider::new(decider(), "k".into());
        let body = p.request_body(&json!({"window": "Gmail"}), &two_questions());
        assert_eq!(body["model"], json!("jev-latest"));
        assert_eq!(body["state"]["window"], json!("Gmail"));
        assert_eq!(body["questions"]["kind"]["type"], json!("choice"));
        assert_eq!(body["questions"]["kind"]["instructions"], json!("which action"));
        assert_eq!(
            body["questions"]["target"]["criteria"]["e1"],
            json!("button \"Send\"")
        );
    }

    #[test]
    fn noul_questions_carry_no_criteria() {
        let p = SystemOneProvider::new(decider(), "k".into());
        let mut q = BTreeMap::new();
        q.insert(
            "destructive".to_string(),
            Question::Noul { instructions: "is this irreversible".into() },
        );
        let body = p.request_body(&json!("state"), &q);
        assert_eq!(body["questions"]["destructive"]["type"], json!("noul"));
        assert!(body["questions"]["destructive"].get("criteria").is_none());
    }

    #[test]
    fn nested_answers_parse() {
        let data = json!({
            "model": "jev-1.13.0",
            "answers": {
                "kind": {"choice": "click", "confidence": 0.99, "probabilities": {"click": 0.99}},
                "target": {"choice": "e1", "confidence": 0.98}
            },
            "usage": {"input_tokens": 1841}
        });
        let a = parse_response(&data, &two_questions(), "jev-latest", 138).unwrap();
        assert_eq!(a.model, "jev-1.13.0");
        assert_eq!(a.input_tokens, 1841);
        assert_eq!(a.choice("kind").unwrap().choice, "click");
        assert_eq!(a.weakest(&["kind", "target"]), 0.98);
        assert_eq!(a.latency_ms, 138);
    }

    #[test]
    fn top_level_answers_parse_too() {
        // The other plausible spelling, carried on purpose so one upstream
        // rename does not take the tool down.
        let data = json!({
            "kind": {"value": "type", "confidence": 0.8},
            "target": {"value": "e1", "confidence": 0.9}
        });
        let a = parse_response(&data, &two_questions(), "jev-latest", 1).unwrap();
        assert_eq!(a.choice("kind").unwrap().choice, "type");
        assert_eq!(a.model, "jev-latest");
    }

    #[test]
    fn a_missing_confidence_is_an_error_not_a_zero() {
        // The point of the whole design: a provider that returns only a label
        // must be rejected loudly, because the thresholds would otherwise
        // become decoration.
        let data = json!({"kind": {"choice": "click"}, "target": {"choice": "e1"}});
        let err = parse_response(&data, &two_questions(), "m", 1).unwrap_err();
        assert!(matches!(err, DecideError::Malformed(_)));
        assert!(err.to_string().contains("calibrated distribution"), "{err}");
    }

    #[test]
    fn a_missing_answer_is_an_error() {
        let data = json!({"kind": {"choice": "click", "confidence": 0.9}});
        assert!(matches!(
            parse_response(&data, &two_questions(), "m", 1),
            Err(DecideError::Malformed(_))
        ));
    }

    #[test]
    fn out_of_range_confidence_is_clamped_not_trusted() {
        let data = json!({
            "kind": {"choice": "click", "confidence": 1.4},
            "target": {"choice": "e1", "confidence": -0.2}
        });
        let a = parse_response(&data, &two_questions(), "m", 1).unwrap();
        assert_eq!(a.choice("kind").unwrap().confidence, 1.0);
        assert_eq!(a.choice("target").unwrap().confidence, 0.0);
        assert_eq!(a.weakest(&["kind", "target"]), 0.0);
    }

    #[test]
    fn an_unconfigured_provider_fails_before_any_network_call() {
        let p = SystemOneProvider::new(decider(), "   ".into());
        assert!(!p.configured());
        let err = p.ask(&json!({}), &two_questions()).unwrap_err();
        assert!(matches!(err, DecideError::Unconfigured));
        assert_eq!(err.code(), "decider_unconfigured");
    }

    #[test]
    fn provider_name_is_provider_slash_model() {
        let mut d = decider();
        d.provider = "cloudflare-clef".into();
        d.model = "clef-flash".into();
        let p = SystemOneProvider::new(d, "k".into());
        assert_eq!(p.name(), "cloudflare-clef/clef-flash");
    }

    #[test]
    fn empty_questions_are_rejected() {
        let p = SystemOneProvider::new(decider(), "k".into());
        assert!(matches!(
            p.ask(&json!({}), &BTreeMap::new()),
            Err(DecideError::Malformed(_))
        ));
    }
}
