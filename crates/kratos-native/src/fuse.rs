//! Three ways of seeing one screen, reconciled.
//!
//! UI Automation knows what things *are* — a button, a text box, what it is
//! called, whether it is enabled — and knows nothing about anything that is
//! drawn rather than declared. OCR knows where the words are and not what
//! they belong to. The detector knows where the controls are and not what
//! they do.
//!
//! So: **UI Automation first, and the others only where it has nothing.**
//! Not "merge everything": an observation that reported a button twice, once
//! from the tree and once as an unlabelled box, would hand the decision model
//! two options that are the same thing and ask it to choose. Two candidates
//! that score alike is the exact failure the broker's `ambiguous` refusal
//! exists to report, and manufacturing it here would be self-inflicted.
//!
//! What makes "the same thing" decidable is overlap: a detector box covering
//! the same rectangle as a UIA control is that control, and the tree's answer
//! is better in every way — it has a role, a name, a container and a value.
//! The box is dropped. Where nothing in the tree covers it, it is a real
//! find and it is kept, with no label, which is honest: nobody knows what it
//! is yet.

use kratos_protocol::Element;

use crate::detect::{iou, Icon};
use crate::ocr::Line;

/// Overlap at which two sightings are one element.
///
/// The spec's number. It is generous on purpose: a detector box drawn tight
/// around an icon and a UIA rectangle that includes its padding overlap by
/// about this much, and treating them as different would double every
/// toolbar.
pub const SAME: f32 = 0.5;

/// Where an element came from, for the `source` field.
fn joined(sources: &[&str]) -> String {
    sources.join("+")
}

/// A tree this sparse is not a tree.
///
/// Spec §10: "if the tree is sparse (<=2 elements), run the pixel path
/// automatically". A canvas app reports its own window and perhaps a scroll
/// bar; a remote desktop reports one pane. Both are windows where the
/// accessibility answer is "there is nothing here", and taking that at face
/// value is how an agent concludes a screen is empty when it is full.
pub const SPARSE: usize = 2;

pub fn is_sparse(uia: &[Element]) -> bool {
    uia.len() <= SPARSE
}

/// How coarsely a fused id locates its element.
///
/// OCR and the detector re-measure a box every pass and the answer moves by a
/// pixel or two, so an id built from exact coordinates would churn. Four
/// pixels is below the gap between adjacent controls -- toolbar icons sit 24
/// or more apart -- and above the jitter, so the same control keeps its id and
/// its neighbour never borrows it.
const CELL: i32 = 4;
/// Which cell of the screen an element's centre falls in.
///
/// The centre, not the corner: a box whose width wobbles as the renderer
/// antialiases keeps its centre and moves both edges.
fn cell(bbox: [i32; 4]) -> (i32, i32) {
    let cx = bbox[0] + bbox[2] / 2;
    let cy = bbox[1] + bbox[3] / 2;
    (cx.div_euclid(CELL), cy.div_euclid(CELL))
}

/// Mint an id for something only the pixels saw.
///
/// **Not the index in the source list.** That was the first implementation and
/// it was wrong in a way that only showed up live: the detector found 33 icons
/// on one pass and 34 on the next, every id after the newcomer shifted by one,
/// and an agent that pinned `ocr:2` and then acted got `stale_id` -- because
/// `act` re-observes to ground the instruction, and the ids it compares against
/// are from that second pass. An id derived from where the thing *is* survives
/// a list that grew.
///
/// `seen` keeps a count per cell so two elements that land in the same four
/// pixels still get distinct ids, in a way that depends on neither's position
/// in the list.
fn mint(prefix: &str, bbox: [i32; 4], seen: &mut Vec<(i32, i32)>) -> String {
    let (x, y) = cell(bbox);
    let n = seen.iter().filter(|c| **c == (x, y)).count();
    seen.push((x, y));
    if n == 0 {
        format!("{prefix}:{x}x{y}")
    } else {
        format!("{prefix}:{x}x{y}#{n}")
    }
}

/// Fuse what three sources saw.
///
/// `uia` leads. `text` and `icons` are added only where nothing in the tree
/// covers them. UIA keeps the id its backend minted, so it stays valid for
/// acting through that backend; the pixel sources get an id derived from where
/// the element is, so a second observation of the same screen names the same
/// element the same way.
pub fn fuse(uia: Vec<Element>, text: &[Line], icons: &[Icon]) -> Vec<Element> {
    let mut out = uia;
    // One tally per source: an icon's id must not change because OCR happened
    // to read, or miss, a caption in the same four pixels.
    let mut text_cells: Vec<(i32, i32)> = Vec::new();
    let mut icon_cells: Vec<(i32, i32)> = Vec::new();
    for e in &mut out {
        if e.source.is_none() {
            e.source = Some("uia".to_string());
        }
    }

    // -- text -----------------------------------------------------------
    //
    // A line of text over a UIA control is that control's own label read
    // twice; over nothing, it is a label with no control, which is still
    // worth reporting -- "the word Send is here" is how a decision model
    // finds a button that is drawn rather than declared.
    for line in text.iter() {
        // Containment, not just overlap. A "Send" label drawn inside a Send
        // button covers a fraction of it -- 0.29 for a 40x14 word in an 80x24
        // control -- so an overlap test alone reports the word as an element
        // of its own and the decision model gets the button twice. Text is
        // small and controls are not: what matters is whether the words are
        // *on* something, not how much of it they cover.
        if out.iter().any(|e| {
            e.bbox.is_some_and(|b| contains(b, line.bbox) || iou(b, line.bbox) > SAME)
        }) {
            continue;
        }
        out.push(Element {
            id: mint("ocr", line.bbox, &mut text_cells),
            role: "text".to_string(),
            label: line.text.clone(),
            bbox: Some(line.bbox),
            source: Some("ocr".to_string()),
            ..Default::default()
        });
    }

    // -- icons ----------------------------------------------------------
    //
    // Done after the text so an icon with a caption under it can take the
    // caption's words as its label: the detector found the control, OCR
    // found what it is called, and together they are one usable element
    // rather than two halves.
    for icon in icons.iter() {
        if out.iter().any(|e| {
            e.bbox.is_some_and(|b| iou(b, icon.bbox) > SAME) && e.source.as_deref() != Some("ocr")
        }) {
            continue;
        }

        // A word inside the box is its name. Not a word merely near it: an
        // icon takes its caption from what is on it, and guessing from
        // proximity is how a Delete icon gets labelled Save.
        let inside = text.iter().find(|l| contains(icon.bbox, l.bbox));
        let (label, source) = match inside {
            Some(l) => (l.text.clone(), joined(&["pixel", "ocr"])),
            None => (String::new(), "pixel".to_string()),
        };

        // A text element the icon has swallowed is now part of it rather than
        // an element of its own.
        if inside.is_some() {
            out.retain(|e| {
                e.source.as_deref() != Some("ocr")
                    || !e.bbox.is_some_and(|b| contains(icon.bbox, b))
            });
        }

        out.push(Element {
            id: mint("pixel", icon.bbox, &mut icon_cells),
            role: if label.is_empty() { "image".to_string() } else { "button".to_string() },
            label,
            bbox: Some(icon.bbox),
            source: Some(source),
            confidence: Some(icon.score as f64),
            ..Default::default()
        });
    }

    out
}

/// Whether `outer` holds `inner`, allowing a couple of pixels of slop.
fn contains(outer: [i32; 4], inner: [i32; 4]) -> bool {
    let slack = 3;
    inner[0] >= outer[0] - slack
        && inner[1] >= outer[1] - slack
        && inner[0] + inner[2] <= outer[0] + outer[2] + slack
        && inner[1] + inner[3] <= outer[1] + outer[3] + slack
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uia(id: &str, role: &str, label: &str, bbox: [i32; 4]) -> Element {
        Element {
            id: id.into(),
            role: role.into(),
            label: label.into(),
            bbox: Some(bbox),
            ..Default::default()
        }
    }

    fn line(text: &str, bbox: [i32; 4]) -> Line {
        Line { text: text.into(), bbox }
    }

    fn icon(bbox: [i32; 4], score: f32) -> Icon {
        Icon { bbox, score }
    }

    #[test]
    fn the_tree_wins_where_it_has_an_answer() {
        // A button the tree knows about, found again by the detector and read
        // again by OCR. One element comes out, and it is the one with a role
        // and a name.
        let button = uia("uia:1:0", "button", "Send", [100, 100, 80, 24]);
        let out = fuse(
            vec![button],
            &[line("Send", [104, 104, 40, 14])],
            &[icon([99, 99, 82, 26], 0.91)],
        );
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(out[0].role, "button");
        assert_eq!(out[0].label, "Send");
        assert_eq!(out[0].source.as_deref(), Some("uia"));
        assert!(out[0].confidence.is_none(), "the tree measures nothing to be confident about");
    }

    #[test]
    fn a_control_only_the_detector_saw_is_kept() {
        let out = fuse(vec![], &[], &[icon([10, 10, 24, 24], 0.77)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source.as_deref(), Some("pixel"));
        assert_eq!(out[0].role, "image", "an icon with no caption is not known to be a button");
        assert_eq!(out[0].label, "", "no label is better than an invented one");
        // A detector score is an f32 widened to an f64, so it compares like
        // one.
        assert!((out[0].confidence.unwrap_or_default() - 0.77).abs() < 1e-6);
    }

    #[test]
    fn an_icon_takes_the_caption_that_is_on_it() {
        // The detector found the control; OCR found what it is called. One
        // element, not two halves.
        let out = fuse(vec![], &[line("Delete", [12, 28, 40, 10])], &[icon([8, 8, 60, 40], 0.8)]);
        assert_eq!(out.len(), 1, "{out:#?}");
        assert_eq!(out[0].label, "Delete");
        assert_eq!(out[0].role, "button");
        assert_eq!(out[0].source.as_deref(), Some("pixel+ocr"));
    }

    #[test]
    fn a_caption_near_an_icon_is_not_its_name() {
        // Guessing from proximity is how a Delete icon gets labelled Save.
        let out = fuse(vec![], &[line("Save", [200, 200, 40, 10])], &[icon([8, 8, 24, 24], 0.8)]);
        assert_eq!(out.len(), 2, "{out:#?}");
        let pixel = out.iter().find(|e| e.source.as_deref() == Some("pixel")).expect("the icon");
        assert_eq!(pixel.label, "");
    }

    #[test]
    fn text_with_nothing_under_it_is_reported() {
        // A label drawn on a canvas belongs to no control, and saying it is
        // there is how a model finds its way around a window with no tree.
        let out = fuse(vec![], &[line("Total: 42", [10, 10, 70, 12])], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "text");
        assert_eq!(out[0].source.as_deref(), Some("ocr"));
    }

    #[test]
    fn a_window_with_nothing_in_the_tree_is_sparse() {
        assert!(is_sparse(&[]));
        assert!(is_sparse(&[uia("a", "pane", "", [0, 0, 10, 10])]));
        assert!(is_sparse(&[
            uia("a", "pane", "", [0, 0, 10, 10]),
            uia("b", "scrollbar", "", [0, 0, 10, 10]),
        ]));
        // Three is a tree.
        assert!(!is_sparse(&[
            uia("a", "pane", "", [0, 0, 10, 10]),
            uia("b", "button", "Go", [0, 0, 10, 10]),
            uia("c", "edit", "Name", [0, 0, 10, 10]),
        ]));
    }

    #[test]
    fn an_element_keeps_its_id_when_the_list_around_it_grows() {
        // The failure this exists for, as it happened: the detector found one
        // more icon on the second pass than the first, and because the ids
        // were list indices every id after the newcomer shifted. An agent
        // that pinned an element and then acted got `stale_id`, because `act`
        // re-observes to ground the instruction and compares against *that*
        // pass.
        let a = icon([300, 300, 24, 24], 0.8);
        let b = icon([400, 300, 24, 24], 0.8);
        let newcomer = icon([100, 300, 24, 24], 0.8);

        let first = fuse(vec![], &[line("Total", [10, 10, 70, 12])], &[a, b]);
        // The newcomer sorts ahead of both, which is what shifted the indices.
        let second =
            fuse(vec![], &[line("Hello", [10, 500, 70, 12]), line("Total", [10, 10, 70, 12])], &[
                newcomer, a, b,
            ]);

        let id_at = |out: &[Element], bbox: [i32; 4]| {
            out.iter().find(|e| e.bbox == Some(bbox)).map(|e| e.id.clone())
        };
        assert_eq!(id_at(&first, a.bbox), id_at(&second, a.bbox), "the icon did not move");
        assert_eq!(id_at(&first, b.bbox), id_at(&second, b.bbox));
        assert_eq!(
            id_at(&first, [10, 10, 70, 12]),
            id_at(&second, [10, 10, 70, 12]),
            "nor did the text, and another line appearing above it is not its business"
        );
        assert!(id_at(&second, newcomer.bbox).is_some(), "{second:#?}");
    }

    #[test]
    fn a_box_that_was_re_measured_by_a_pixel_keeps_its_id() {
        // OCR and the detector re-measure every pass and disagree with
        // themselves slightly. An id that churned on that would be no better
        // than an index.
        let first = fuse(vec![], &[], &[icon([300, 300, 24, 24], 0.8)]);
        let second = fuse(vec![], &[], &[icon([302, 301, 22, 23], 0.8)]);
        assert_eq!(first[0].id, second[0].id);
        // A cell has edges, so a centre that jitters across one does get a
        // new id -- 312 is a boundary, and 311 is the cell below. That case
        // is not papered over here: the broker re-pins a pixel element by
        // position when its id is gone, which is what the id meant anyway.
        // See `repin_by_position` in the broker's act.rs.
        let moved = fuse(vec![], &[], &[icon([301, 299, 23, 25], 0.8)]);
        assert_ne!(first[0].id, moved[0].id);
    }

    #[test]
    fn two_elements_in_the_same_few_pixels_still_get_two_ids() {
        // A cell is four pixels across, so a collision is possible. What it
        // must never do is give two elements one id -- an agent pinning the
        // second would act on the first.
        // Same centre, so the same cell; barely any overlap, so neither is
        // the other seen twice.
        let out =
            fuse(vec![], &[], &[icon([300, 300, 24, 24], 0.8), icon([310, 290, 4, 44], 0.4)]);
        assert_eq!(out.len(), 2, "{out:#?}");
        assert_ne!(out[0].id, out[1].id);
    }

    #[test]
    fn every_fused_element_says_where_it_came_from() {
        let out = fuse(
            vec![uia("uia:1:0", "edit", "Name", [0, 0, 100, 20])],
            &[line("elsewhere", [500, 500, 60, 12])],
            &[icon([300, 300, 20, 20], 0.6)],
        );
        assert_eq!(out.len(), 3, "{out:#?}");
        for e in &out {
            let source = e.source.as_deref().unwrap_or("");
            assert!(
                ["uia", "ocr", "pixel", "pixel+ocr"].contains(&source),
                "{source:?} is not a source a consumer can branch on"
            );
        }
        // And a confidence only where something measured one.
        assert!(out.iter().filter(|e| e.confidence.is_some()).count() == 1);
    }
}
