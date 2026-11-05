//! Structural difference between two observations.
//!
//! What appeared, what vanished, what changed state, and whether focus moved.
//! This answers "what changed" mechanically, which is a different and
//! complementary question from `verify`, where the decision model is asked
//! whether the screen now supports a claim.
//!
//! Elements are keyed by `(label, role, container)` rather than by id, because
//! ids are regenerated on every observation and would make every element look
//! new. The container is part of the key for the same reason it is part of the
//! criterion: without it, a button moving between two dialogs looks like no
//! change at all.

use kratos_protocol::{BackendObservation, Element, StructuralDiff};

fn key(e: &Element) -> (String, String, String) {
    (
        e.label.trim().to_string(),
        e.role.clone(),
        e.container.clone().unwrap_or_default(),
    )
}

pub fn diff(before: &BackendObservation, after: &BackendObservation) -> StructuralDiff {
    use std::collections::HashMap;

    let mut b: HashMap<_, &Element> = HashMap::new();
    for e in &before.elements {
        b.insert(key(e), e);
    }
    let mut a: HashMap<_, &Element> = HashMap::new();
    for e in &after.elements {
        a.insert(key(e), e);
    }

    let added = a.keys().filter(|k| !b.contains_key(*k)).count();
    let removed = b.keys().filter(|k| !a.contains_key(*k)).count();
    let changed = a
        .iter()
        .filter(|(k, av)| {
            b.get(*k).is_some_and(|bv| {
                bv.enabled != av.enabled || bv.value.as_deref() != av.value.as_deref()
            })
        })
        .count();

    StructuralDiff {
        added_count: added,
        removed_count: removed,
        changed_count: changed,
        focus_changed: before.window != after.window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(window: &str, els: Vec<Element>) -> BackendObservation {
        BackendObservation {
            window: window.into(),
            url: None,
            elements: els,
            truncated: false,
        }
    }

    fn el(id: &str, label: &str, container: Option<&str>) -> Element {
        Element {
            id: id.into(),
            role: "button".into(),
            label: label.into(),
            container: container.map(|c| c.into()),
            ..Default::default()
        }
    }

    #[test]
    fn an_unchanged_screen_shows_nothing() {
        let a = obs("Gmail", vec![el("e1", "Send", None), el("e2", "Discard", None)]);
        // Same screen, freshly observed: ids differ because a backend
        // regenerates them, and that must not read as churn.
        let b = obs("Gmail", vec![el("x9", "Send", None), el("x8", "Discard", None)]);
        let d = diff(&a, &b);
        assert!(!d.touched(), "{d:?}");
        assert_eq!((d.added_count, d.removed_count, d.changed_count), (0, 0, 0));
    }

    #[test]
    fn a_dialog_closing_shows_as_removals() {
        let before = obs("Gmail", vec![el("e1", "Send", Some("dialog:New Message")), el("e2", "Inbox", None)]);
        let after = obs("Gmail", vec![el("e2", "Inbox", None)]);
        let d = diff(&before, &after);
        assert_eq!(d.removed_count, 1);
        assert_eq!(d.added_count, 0);
        assert!(d.touched());
    }

    #[test]
    fn focus_moving_counts_even_with_identical_elements() {
        let a = obs("Gmail - Brave", vec![el("e1", "Send", None)]);
        let b = obs("Untitled - Notepad", vec![el("e1", "Send", None)]);
        assert!(diff(&a, &b).focus_changed);
        assert!(diff(&a, &b).touched());
    }

    #[test]
    fn a_value_change_is_a_change() {
        let mut before_el = el("e1", "Subject", None);
        before_el.value = Some(String::new());
        let mut after_el = el("e1", "Subject", None);
        after_el.value = Some("Order-taking AI agents".into());
        let d = diff(&obs("Gmail", vec![before_el]), &obs("Gmail", vec![after_el]));
        assert_eq!(d.changed_count, 1);
        assert!(d.touched());
    }

    #[test]
    fn an_enabled_state_change_is_a_change() {
        let mut before_el = el("e1", "Send", None);
        before_el.enabled = false;
        let after_el = el("e1", "Send", None);
        let d = diff(&obs("G", vec![before_el]), &obs("G", vec![after_el]));
        assert_eq!(d.changed_count, 1);
    }

    #[test]
    fn the_container_is_part_of_the_identity() {
        // The same button in a different dialog is a different thing, and a
        // diff that ignored containers would report no change at all.
        let a = obs("G", vec![el("e1", "Send", Some("dialog:New Message"))]);
        let b = obs("G", vec![el("e1", "Send", Some("dialog:Reply"))]);
        let d = diff(&a, &b);
        assert_eq!((d.added_count, d.removed_count), (1, 1));
    }

    #[test]
    fn an_empty_before_and_after_is_not_a_change() {
        assert!(!diff(&obs("G", vec![]), &obs("G", vec![])).touched());
    }

    #[test]
    fn duplicate_elements_do_not_inflate_the_counts() {
        // Two identically-presented controls collapse to one key, so a screen
        // that reports them twice does not look like it gained an element.
        let a = obs("G", vec![el("e1", "Amount", None), el("e2", "Amount", None)]);
        let b = obs("G", vec![el("e3", "Amount", None)]);
        assert!(!diff(&a, &b).touched());
    }
}
