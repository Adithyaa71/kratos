//! Doing it: the control's own interface first, the mouse and keyboard second.
//!
//! A click delivered through `InvokePattern` is the button being pressed. A
//! click delivered at a coordinate is a guess that the button is still where
//! it was a moment ago, and it is a guess that can land on whatever moved
//! into that rectangle instead. So every action tries the control's own
//! interface first, and the result says which happened -- `semantic` or
//! `physical` -- because an action that quietly became a guess is not the
//! action that was asked for.
//!
//! The fallback is not a weakness to hide. Most of a Windows desktop exposes
//! no pattern for most things, and a backend that refused to click anything
//! it could not invoke would be useless on half the machine. It is reported
//! instead.

use windows::core::BSTR;
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern,
    IUIAutomationTogglePattern, IUIAutomationValuePattern, ScrollAmount_LargeDecrement,
    ScrollAmount_LargeIncrement, ScrollAmount_NoAmount, UIA_ExpandCollapsePatternId,
    UIA_InvokePatternId, UIA_ScrollPatternId, UIA_SelectionItemPatternId, UIA_TogglePatternId,
    UIA_ValuePatternId, UIA_ValueValuePropertyId,
};

use crate::input;

/// What the broker asked for, reduced to what this backend needs.
#[derive(Debug, Clone)]
pub struct Request {
    pub kind: String,
    /// `uia:<hwnd>:<path>`, when the action has a target. `key` and an
    /// unqualified `scroll` have none: a keypress goes to whatever has focus.
    pub path: Option<Vec<usize>>,
    pub hwnd: isize,
    /// Where the element was when it was observed, for the physical path.
    pub bbox: Option<[i32; 4]>,
    pub text: Option<String>,
    pub key: Option<String>,
    pub amount: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub applied: bool,
    /// The control's value afterwards, when it has one. The broker's
    /// read-back evidence is this field.
    pub readback: Option<String>,
    /// `semantic` or `physical`.
    pub method: &'static str,
    pub detail: Option<String>,
}

/// Parse `uia:<hwnd>:<a.b.c>` back into somewhere to look.
pub fn parse_id(id: &str) -> Option<(isize, Vec<usize>)> {
    let mut parts = id.split(':');
    if parts.next()? != "uia" {
        return None;
    }
    let hwnd = isize::from_str_radix(parts.next()?, 16).ok()?;
    let path = parts.next()?;
    let indices: Option<Vec<usize>> =
        path.split('.').filter(|s| !s.is_empty()).map(|s| s.parse().ok()).collect();
    Some((hwnd, indices?))
}

/// Walk back down to the element an id names.
///
/// The same filter the observation used, walked live rather than from the
/// cache: a pattern needs a real element, and the cache deliberately holds
/// none. The broker re-observes immediately before acting, so this path is as
/// fresh as the ids it came from -- and if the tree has changed under it, the
/// walk runs out of children and the action is refused rather than landing on
/// whatever is at that index now.
pub unsafe fn find(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    path: &[usize],
) -> Option<IUIAutomationElement> {
    let filter = crate::uia::interactive_condition(automation).ok()?;
    let walker = automation.CreateTreeWalker(&filter).ok()?;
    let mut here = root.clone();
    for index in path {
        let mut child = walker.GetFirstChildElement(&here).ok()?;
        for _ in 0..*index {
            child = walker.GetNextSiblingElement(&child).ok()?;
        }
        here = child;
    }
    Some(here)
}

/// Try the control's own interface.
///
/// `None` means there was no pattern for this kind of action, which is not a
/// failure: it is the signal to use the mouse.
unsafe fn semantic(element: &IUIAutomationElement, req: &Request) -> Option<Outcome> {
    let value_now = || -> Option<String> {
        element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()?
            .CurrentValue()
            .ok()
            .map(|v| v.to_string())
    };

    match req.kind.as_str() {
        "click" => {
            if let Ok(invoke) =
                element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            {
                invoke.Invoke().ok()?;
                return Some(Outcome {
                    applied: true,
                    method: "semantic",
                    detail: Some("InvokePattern".into()),
                    ..Default::default()
                });
            }
            // A checkbox has no Invoke; toggling it is what clicking it does.
            if let Ok(toggle) =
                element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
            {
                toggle.Toggle().ok()?;
                return Some(Outcome {
                    applied: true,
                    method: "semantic",
                    detail: Some("TogglePattern".into()),
                    ..Default::default()
                });
            }
            // A list item, a tab, a radio button: clicking means selecting.
            if let Ok(item) = element
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            {
                item.Select().ok()?;
                return Some(Outcome {
                    applied: true,
                    method: "semantic",
                    detail: Some("SelectionItemPattern".into()),
                    ..Default::default()
                });
            }
            // A combo box or a tree node: clicking opens it.
            if let Ok(expand) = element
                .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
            {
                expand.Expand().ok()?;
                return Some(Outcome {
                    applied: true,
                    method: "semantic",
                    detail: Some("ExpandCollapsePattern".into()),
                    ..Default::default()
                });
            }
            None
        }

        "type" => {
            let text = req.text.as_deref()?;
            let value = element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()?;
            // A read-only control reports the pattern and refuses the write,
            // which is a refusal worth passing up rather than retrying with
            // the keyboard -- typing into it would go somewhere else.
            if value.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(false) {
                return Some(Outcome {
                    applied: false,
                    method: "semantic",
                    detail: Some("the control is read-only".into()),
                    ..Default::default()
                });
            }
            let _ = element.SetFocus();
            value.SetValue(&BSTR::from(text)).ok()?;
            Some(Outcome {
                applied: true,
                readback: value_now(),
                method: "semantic",
                detail: Some("ValuePattern".into()),
            })
        }

        "select" => {
            let item = element
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .ok()?;
            item.Select().ok()?;
            Some(Outcome {
                applied: true,
                readback: value_now(),
                method: "semantic",
                detail: Some("SelectionItemPattern".into()),
            })
        }

        "focus" => {
            element.SetFocus().ok()?;
            Some(Outcome {
                applied: true,
                method: "semantic",
                detail: Some("SetFocus".into()),
                ..Default::default()
            })
        }

        "scroll" => {
            let scroll =
                element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId).ok()?;
            let down = req.amount.unwrap_or(3) >= 0;
            let vertical =
                if down { ScrollAmount_LargeIncrement } else { ScrollAmount_LargeDecrement };
            scroll.Scroll(ScrollAmount_NoAmount, vertical).ok()?;
            Some(Outcome {
                applied: true,
                method: "semantic",
                detail: Some("ScrollPattern".into()),
                ..Default::default()
            })
        }

        _ => None,
    }
}

/// The mouse and the keyboard, for everything with no pattern behind it.
fn physical(req: &Request) -> Outcome {
    match req.kind.as_str() {
        "click" => match req.bbox {
            Some([x, y, w, h]) => {
                input::click(x + w / 2, y + h / 2);
                Outcome {
                    applied: true,
                    method: "physical",
                    detail: Some("click at the centre of where it was".into()),
                    ..Default::default()
                }
            }
            None => Outcome {
                applied: false,
                method: "physical",
                detail: Some("no pattern to invoke and no rectangle to aim at".into()),
                ..Default::default()
            },
        },

        "type" => match &req.text {
            Some(text) => {
                input::type_text(text);
                Outcome {
                    applied: true,
                    method: "physical",
                    detail: Some(input::how_typed(text).into()),
                    ..Default::default()
                }
            }
            None => Outcome {
                applied: false,
                method: "physical",
                detail: Some("nothing to type".into()),
                ..Default::default()
            },
        },

        "key" => match req.key.as_deref().and_then(input::key_chord) {
            Some(chord) => {
                input::press(&chord);
                Outcome {
                    applied: true,
                    method: "physical",
                    detail: Some(format!("pressed {:?}", req.key.as_deref().unwrap_or(""))),
                    ..Default::default()
                }
            }
            None => Outcome {
                applied: false,
                method: "physical",
                detail: Some(format!(
                    "{:?} is not a key this build knows how to press",
                    req.key.as_deref().unwrap_or("")
                )),
                ..Default::default()
            },
        },

        "scroll" => {
            if let Some([x, y, w, h]) = req.bbox {
                input::move_to(x + w / 2, y + h / 2);
            }
            input::scroll(req.amount.unwrap_or(3));
            Outcome {
                applied: true,
                method: "physical",
                detail: Some("wheel".into()),
                ..Default::default()
            }
        }

        other => Outcome {
            applied: false,
            method: "physical",
            detail: Some(format!("{other:?} is not something this backend can do")),
            ..Default::default()
        },
    }
}

/// Carry out one action. Called on the apartment thread.
pub unsafe fn perform(
    automation: &IUIAutomation,
    req: &Request,
) -> Result<Outcome, String> {
    // A keypress has no element by design: it goes to whatever has focus. A
    // backend that refused a null target would break `press Enter` entirely,
    // which is exactly what both of the earlier ones did.
    let element = match &req.path {
        Some(path) => {
            let root = automation
                .ElementFromHandle(windows::Win32::Foundation::HWND(req.hwnd as *mut _))
                .map_err(|e| format!("no element for that window: {e}"))?;
            match find(automation, &root, path) {
                Some(e) => Some(e),
                None => {
                    return Err(
                        "that element is not where it was; observe again and re-pin".to_string()
                    )
                }
            }
        }
        None => None,
    };

    if let Some(element) = &element {
        if let Some(outcome) = semantic(element, req) {
            return Ok(outcome);
        }
    }

    let mut outcome = physical(req);
    // Read the value back even when the keyboard did the typing: the broker's
    // strongest evidence is the field containing what was asked for, and it
    // does not care how it got there.
    if outcome.applied && req.kind == "type" {
        if let Some(element) = &element {
            outcome.readback = element
                .GetCurrentPropertyValue(UIA_ValueValuePropertyId)
                .ok()
                .and_then(|v| crate::uia::variant_string(&v));
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_says_which_window_and_which_path() {
        assert_eq!(parse_id("uia:1a2b:0.3.7"), Some((0x1a2b, vec![0, 3, 7])));
        // The root of a window: a path with nothing in it.
        assert_eq!(parse_id("uia:ff:"), Some((0xff, vec![])));
        // Another backend's id is not this backend's to act on.
        assert!(parse_id("brave:w1:t1:f0:e2").is_none());
        assert!(parse_id("uia:notahandle:0").is_none());
        assert!(parse_id("ocr:1a2b:4").is_none());
    }
}
