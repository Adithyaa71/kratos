//! Turning a screen full of elements into a choice the model can answer.
//!
//! Two problems to solve, and the second is the one that bit in production.
//!
//! **Cardinality.** A choice admits at most 255 options. A busy page reports
//! more elements than that, so the list is narrowed -- but one slot is always
//! reserved for an explicit "none of these" escape, so an overflow surfaces as
//! low confidence rather than as a forced wrong pick.
//!
//! **Ambiguity.** Two controls that present identically give the model no
//! basis to choose between them, and it will still return a label, with high
//! confidence, because one of them genuinely is the best answer to the
//! question as posed. That is how a Gmail compose produced confidence 1.00 for
//! the wrong field and typed a subject line into the recipient box. The fix is
//! not better thresholds: it is making sure no two options read the same.
//! `Element::criterion` supplies the container, and anything still colliding
//! after that gets a positional qualifier here.

use kratos_protocol::Element;

/// The reserved escape option. Named so it cannot collide with an element id.
pub const NONE_OF_THESE: &str = "__none_of_these__";

pub const NONE_OF_THESE_DESCRIPTION: &str =
    "None of the elements listed above is what the instruction refers to";

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The element id, or [`NONE_OF_THESE`].
    pub id: String,
    /// How the option is described to the model.
    pub criterion: String,
}

/// Words that carry no signal when scoring relevance.
const STOPWORDS: [&str; 24] = [
    "the", "a", "an", "and", "or", "in", "into", "on", "at", "to", "of", "for", "with", "then",
    "that", "this", "it", "its", "is", "be", "click", "type", "press", "button",
];

fn signal_words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Build the option list for the `target` question.
///
/// Returns the candidates -- always ending with the escape hatch -- and
/// whether anything was dropped to fit.
pub fn compress(instruction: &str, elements: &[Element], cap: usize) -> (Vec<Candidate>, bool) {
    // One slot is the escape hatch's, always.
    let room = cap.saturating_sub(1);
    let wanted = signal_words(instruction);

    let mut kept: Vec<(usize, &Element)> = elements.iter().enumerate().collect();
    let truncated = kept.len() > room;

    if truncated {
        // Narrow by lexical overlap with the instruction, keeping document
        // order among equals so the list stays stable between observations.
        kept.sort_by(|a, b| {
            let sa = relevance(&wanted, a.1);
            let sb = relevance(&wanted, b.1);
            sb.cmp(&sa).then(a.0.cmp(&b.0))
        });
        kept.truncate(room);
        kept.sort_by_key(|(i, _)| *i);
    }

    let mut out: Vec<Candidate> = kept
        .iter()
        .map(|(_, e)| Candidate { id: e.id.clone(), criterion: e.criterion() })
        .collect();

    disambiguate(&mut out, &kept.iter().map(|(_, e)| *e).collect::<Vec<_>>());

    out.push(Candidate {
        id: NONE_OF_THESE.to_string(),
        criterion: NONE_OF_THESE_DESCRIPTION.to_string(),
    });
    (out, truncated)
}

fn relevance(wanted: &[String], e: &Element) -> usize {
    let hay = format!("{} {} {}", e.label, e.role, e.container.clone().unwrap_or_default())
        .to_lowercase();
    wanted.iter().filter(|w| hay.contains(w.as_str())).count()
}

/// Give colliding options something to tell them apart by.
///
/// A positional qualifier is a real distinction a model can reason about and
/// an operator can predict -- "the lower one" is how a person would describe
/// it too. Appending a bare index would not be: `(2 of 3)` tells the model
/// nothing about which one the instruction means, and would launder a
/// genuinely ambiguous screen into a confident answer.
///
/// Options with no bbox cannot be qualified this way, so they are left
/// identical on purpose: the model then has to split its probability between
/// them, confidence drops, and the action is refused rather than guessed.
fn disambiguate(candidates: &mut [Candidate], elements: &[&Element]) {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for c in candidates.iter() {
        *counts.entry(c.criterion.as_str()).or_insert(0) += 1;
    }
    let colliding: Vec<String> =
        counts.iter().filter(|(_, n)| **n > 1).map(|(k, _)| k.to_string()).collect();
    if colliding.is_empty() {
        return;
    }

    let bounds = overall_bounds(elements);
    for (c, e) in candidates.iter_mut().zip(elements.iter()) {
        if !colliding.contains(&c.criterion) {
            continue;
        }
        if let (Some(bbox), Some(b)) = (e.bbox, bounds) {
            c.criterion = format!("{} ({})", c.criterion, position_phrase(bbox, b));
        }
    }
}

type Bounds = (i32, i32, i32, i32);

fn overall_bounds(elements: &[&Element]) -> Option<Bounds> {
    let mut acc: Option<Bounds> = None;
    for e in elements {
        if let Some([x, y, w, h]) = e.bbox {
            let (x0, y0, x1, y1) = (x, y, x + w, y + h);
            acc = Some(match acc {
                None => (x0, y0, x1, y1),
                Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
            });
        }
    }
    acc
}

fn position_phrase(bbox: [i32; 4], bounds: Bounds) -> String {
    let [x, y, w, h] = bbox;
    let (cx, cy) = (x + w / 2, y + h / 2);
    let (bx0, by0, bx1, by1) = bounds;
    let vertical = third(cy, by0, by1, ["upper", "middle", "lower"]);
    let horizontal = third(cx, bx0, bx1, ["left", "centre", "right"]);
    format!("{vertical} {horizontal} of the screen")
}

fn third(v: i32, lo: i32, hi: i32, names: [&str; 3]) -> &str {
    let span = (hi - lo).max(1);
    let frac = (v - lo) as f64 / span as f64;
    if frac < 1.0 / 3.0 {
        names[0]
    } else if frac < 2.0 / 3.0 {
        names[1]
    } else {
        names[2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(id: &str, role: &str, label: &str, container: Option<&str>, bbox: [i32; 4]) -> Element {
        Element {
            id: id.into(),
            role: role.into(),
            label: label.into(),
            container: container.map(|c| c.into()),
            bbox: Some(bbox),
            ..Default::default()
        }
    }

    #[test]
    fn the_escape_hatch_is_always_last_and_always_present() {
        let (c, truncated) = compress("click send", &[], crate::MAX_CARDINALITY);
        assert!(!truncated);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].id, NONE_OF_THESE);
    }

    #[test]
    fn the_escape_hatch_costs_a_slot_so_the_cap_is_never_exceeded() {
        // 255 options plus an escape hatch would be 256 -- one over the
        // documented ceiling, which is how this overflowed once before.
        let many: Vec<Element> = (0..400)
            .map(|i| el(&format!("e{i}"), "button", &format!("b{i}"), None, [i, i, 10, 10]))
            .collect();
        let (c, truncated) = compress("click b7", &many, crate::MAX_CARDINALITY);
        assert!(truncated);
        assert_eq!(c.len(), crate::MAX_CARDINALITY);
        assert_eq!(c.last().unwrap().id, NONE_OF_THESE);
    }

    #[test]
    fn narrowing_keeps_the_relevant_element_and_preserves_document_order() {
        let mut many: Vec<Element> = (0..300)
            .map(|i| el(&format!("e{i}"), "button", &format!("filler{i}"), None, [i, i, 10, 10]))
            .collect();
        many.push(el("needle", "button", "Reconcile Invoice", None, [5, 5, 80, 20]));
        let (c, truncated) = compress("click the \"Reconcile Invoice\" button", &many, 50);
        assert!(truncated);
        assert!(c.iter().any(|x| x.id == "needle"), "the relevant element must survive");

        // Order among the survivors follows the original list, so ids do not
        // shuffle between two observations of the same screen.
        let kept: Vec<&str> = c.iter().map(|x| x.id.as_str()).collect();
        let positions: Vec<usize> = kept
            .iter()
            .filter(|id| **id != NONE_OF_THESE)
            .filter_map(|id| many.iter().position(|e| e.id == *id))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    }

    // -- the Gmail failure --------------------------------------------------

    #[test]
    fn a_container_separates_two_fields_with_the_same_label() {
        // This is the real defect: on a Gmail compose, the search box and the
        // recipient box both present as a textbox labelled "To" or similar.
        // With containers they are distinct, and no positional hack is needed.
        let els = vec![
            el("e1", "textbox", "To recipients", Some("dialog:New Message"), [400, 260, 300, 20]),
            el("e2", "textbox", "To recipients", Some("region:Search results"), [400, 80, 300, 20]),
        ];
        let (c, _) = compress("type the address into the To field", &els, 255);
        assert_ne!(c[0].criterion, c[1].criterion);
        assert!(c[0].criterion.contains("New Message"));
        assert!(c[1].criterion.contains("Search results"));
        // No positional qualifier was needed, so none was added.
        assert!(!c[0].criterion.contains("of the screen"));
    }

    #[test]
    fn identical_options_get_a_positional_qualifier() {
        let els = vec![
            el("e1", "textbox", "Amount", None, [100, 100, 200, 20]),
            el("e2", "textbox", "Amount", None, [100, 900, 200, 20]),
        ];
        let (c, _) = compress("type \"50\" into the amount box", &els, 255);
        assert_ne!(c[0].criterion, c[1].criterion);
        assert!(c[0].criterion.contains("upper"), "{}", c[0].criterion);
        assert!(c[1].criterion.contains("lower"), "{}", c[1].criterion);
        // And the qualifier is descriptive, not a bare index the model cannot
        // reason about.
        assert!(!c[0].criterion.contains("1 of"));
    }

    #[test]
    fn unqualifiable_duplicates_are_left_identical_so_confidence_falls() {
        // Without a bbox there is no honest way to tell them apart. Leaving
        // them identical splits the model's probability mass and produces a
        // refusal, which is the correct outcome for an ambiguous screen --
        // far better than inventing a distinction.
        let els = vec![
            Element { id: "e1".into(), role: "textbox".into(), label: "Amount".into(), ..Default::default() },
            Element { id: "e2".into(), role: "textbox".into(), label: "Amount".into(), ..Default::default() },
        ];
        let (c, _) = compress("type \"50\"", &els, 255);
        assert_eq!(c[0].criterion, c[1].criterion);
    }

    #[test]
    fn three_way_collision_spreads_across_thirds() {
        let els = vec![
            el("e1", "button", "Go", None, [10, 10, 20, 20]),
            el("e2", "button", "Go", None, [10, 500, 20, 20]),
            el("e3", "button", "Go", None, [10, 1000, 20, 20]),
        ];
        let (c, _) = compress("click go", &els, 255);
        let criteria: Vec<&str> = c.iter().map(|x| x.criterion.as_str()).collect();
        assert_eq!(
            criteria.iter().collect::<std::collections::HashSet<_>>().len(),
            criteria.len(),
            "every option must read differently: {criteria:?}"
        );
    }

    #[test]
    fn unlabelled_elements_are_still_offered() {
        let els = vec![el("e1", "button", "   ", None, [10, 10, 20, 20])];
        let (c, _) = compress("click it", &els, 255);
        assert!(c[0].criterion.contains("unlabelled"));
    }

    #[test]
    fn ids_are_opaque_and_survive_untouched() {
        let els = vec![el("brave:w3:t17:e42", "button", "Send", Some("dialog:New Message"), [1, 2, 3, 4])];
        let (c, _) = compress("click send", &els, 255);
        assert_eq!(c[0].id, "brave:w3:t17:e42");
    }
}
