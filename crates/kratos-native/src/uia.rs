//! UI Automation, read in one trip.
//!
//! The Python backend read a window by walking it element by element and
//! asking for each property as it went, and every one of those is a
//! cross-process call into the application being inspected: 24 elements in
//! 223ms, 168 in 2.1s. The cost is not the tree, it is the round trips.
//!
//! So this asks once. `BuildUpdatedCache` with a tree scope and a filter
//! brings back the whole subtree *and* the properties in a single call, and
//! everything after that -- names, roles, rectangles, parents -- is read out
//! of the local cache with no further calls into the other process.
//!
//! Two rules follow from that and are worth stating, because breaking either
//! quietly restores the old cost:
//!
//! * **Never touch a non-cached accessor.** `element.Name()` is a call;
//!   `element.CachedName()` is a read. The first is how a 70ms observe becomes
//!   a 2-second one.
//! * **Prune in the condition, not afterwards.** An offscreen or non-control
//!   element filtered out in Rust has already been marshalled across.
//!
//! COM lives on its own thread, in the multithreaded apartment: UIA calls
//! block, and a blocked STA pumps no messages.

use std::sync::mpsc;

use windows::Win32::Foundation::{HWND, RECT, VARIANT_BOOL};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_BSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    AutomationElementMode_None, CUIAutomation8, IUIAutomation, IUIAutomationCacheRequest,
    IUIAutomationElement, IUIAutomationElementArray, TreeScope_Subtree, UIA_AutomationIdPropertyId,
    UIA_BoundingRectanglePropertyId, UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
    UIA_IsControlElementPropertyId, UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId,
    UIA_IsOffscreenPropertyId, UIA_IsPasswordPropertyId, UIA_NamePropertyId,
    UIA_PROPERTY_ID, UIA_ValueValuePropertyId,
};

// ---------------------------------------------------------------------------
// VARIANTs
// ---------------------------------------------------------------------------
//
// Two of them, by hand. The `windows` crate exposes the raw union here with no
// conversions, and the alternative -- caching patterns instead of property
// values -- needs `AutomationElementMode_Full`, which keeps a live reference
// to every element in the window. Twenty lines of contained unsafe is the
// cheaper trade.

/// A `VARIANT` holding a boolean, for a property condition.
fn variant_bool(b: bool) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        let inner = &mut v.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        // VARIANT_BOOL is -1 for true: the one place in Win32 where a boolean
        // is not 1.
        inner.Anonymous.boolVal = VARIANT_BOOL(if b { -1 } else { 0 });
    }
    v
}

/// The string in a `VARIANT`, if it holds one.
///
/// Borrowed, not taken: the VARIANT owns the BSTR and frees it, so this
/// copies out rather than moving the pointer.
fn variant_string(v: &VARIANT) -> Option<String> {
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        if inner.vt != VT_BSTR {
            return None;
        }
        let bstr = &*inner.Anonymous.bstrVal;
        Some(bstr.to_string())
    }
}

/// What one element looks like once it is out of the cache.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Found {
    pub id: String,
    pub role: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[i32; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Observation {
    pub window: String,
    pub elements: Vec<Found>,
    pub truncated: bool,
    /// How long the single cached fetch took, separately from flattening it.
    pub fetch_ms: u64,
    pub walk_ms: u64,
}

/// How many elements one observation may carry.
///
/// A dense window -- an IDE, a spreadsheet with every cell a control -- has
/// thousands, and a decision model is given a compressed candidate list of a
/// few dozen. Past this, more elements buy nothing and cost marshalling.
pub const MAX_ELEMENTS: usize = 400;

enum Job {
    Observe { hwnd: isize, reply: mpsc::Sender<Result<Observation, String>> },
}

/// A handle to the thread that owns COM.
#[derive(Clone)]
pub struct Uia {
    jobs: mpsc::Sender<Job>,
}

impl Uia {
    /// Start the apartment thread. Fails only if COM itself will not start.
    pub fn start() -> Result<Uia, String> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("kratos-uia".into())
            .spawn(move || unsafe {
                // Multithreaded: a UIA call blocks, and a blocked
                // single-threaded apartment stops pumping the messages that
                // some providers need to answer.
                let init = CoInitializeEx(None, COINIT_MULTITHREADED);
                if init.is_err() {
                    let _ = ready_tx.send(Err(format!("CoInitializeEx: {init:?}")));
                    return;
                }
                let automation: IUIAutomation =
                    match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
                        Ok(a) => a,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("CUIAutomation8: {e}")));
                            CoUninitialize();
                            return;
                        }
                    };
                let _ = ready_tx.send(Ok(()));

                while let Ok(job) = rx.recv() {
                    match job {
                        Job::Observe { hwnd, reply } => {
                            let _ = reply.send(observe_window(&automation, hwnd));
                        }
                    }
                }
                CoUninitialize();
            })
            .map_err(|e| format!("could not start the UIA thread: {e}"))?;

        ready_rx
            .recv()
            .map_err(|_| "the UIA thread died before it was ready".to_string())??;
        Ok(Uia { jobs: tx })
    }

    pub fn observe(&self, hwnd: isize) -> Result<Observation, String> {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .send(Job::Observe { hwnd, reply: tx })
            .map_err(|_| "the UIA thread has gone".to_string())?;
        rx.recv().map_err(|_| "the UIA thread dropped the request".to_string())?
    }
}

// ---------------------------------------------------------------------------
// The one fetch
// ---------------------------------------------------------------------------

unsafe fn build_cache(automation: &IUIAutomation) -> windows::core::Result<IUIAutomationCacheRequest> {
    let cache = automation.CreateCacheRequest()?;

    // Exactly the properties the protocol reports, and no others: every one
    // of them is marshalled for every element.
    for id in [
        UIA_NamePropertyId,
        UIA_ControlTypePropertyId,
        UIA_IsEnabledPropertyId,
        UIA_BoundingRectanglePropertyId,
        UIA_AutomationIdPropertyId,
        UIA_ClassNamePropertyId,
        UIA_ValueValuePropertyId,
        UIA_IsPasswordPropertyId,
        UIA_IsKeyboardFocusablePropertyId,
        UIA_IsOffscreenPropertyId,
    ] {
        cache.AddProperty(id)?;
    }

    // The cache holds the *tree*, not a flat list, so a container can be read
    // from an element's cached parent without another call.
    cache.SetTreeScope(TreeScope_Subtree)?;

    // Pruned here rather than in Rust: an element filtered out afterwards has
    // already been marshalled across the process boundary, which is the cost
    // this whole design is about.
    //
    // `IsControlElement` is UIA's own answer to "would a user consider this a
    // thing on the screen", which is a better filter than a list of control
    // types -- and it is the one the provider computes, so it costs nothing.
    let is_control = automation
        .CreatePropertyCondition(UIA_IsControlElementPropertyId, &variant_bool(true))?;
    let on_screen =
        automation.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &variant_bool(false))?;
    let filter = automation.CreateAndCondition(&is_control, &on_screen)?;
    cache.SetTreeFilter(&filter)?;

    // None, not Full: a cached-only element cannot be used for patterns, and
    // nothing here needs to be -- acting re-finds its target by path. Full
    // keeps a live reference to every element in the window, which is both
    // slower to build and a lot to hold.
    cache.SetAutomationElementMode(AutomationElementMode_None)?;
    Ok(cache)
}

unsafe fn observe_window(automation: &IUIAutomation, hwnd: isize) -> Result<Observation, String> {
    let started = std::time::Instant::now();
    let root = automation
        .ElementFromHandle(HWND(hwnd as *mut _))
        .map_err(|e| format!("no UI Automation element for that window: {e}"))?;
    let cache = build_cache(automation).map_err(|e| format!("cache request: {e}"))?;

    // The one trip. Everything below this line is local.
    let cached = root
        .BuildUpdatedCache(&cache)
        .map_err(|e| format!("BuildUpdatedCache: {e}"))?;
    let fetch_ms = started.elapsed().as_millis() as u64;

    let walk_started = std::time::Instant::now();
    let window = cached.CachedName().map(|n| n.to_string()).unwrap_or_default();
    let mut out = Observation {
        window,
        elements: Vec::new(),
        truncated: false,
        fetch_ms,
        walk_ms: 0,
    };
    let mut path = Vec::new();
    flatten(&cached, hwnd, &mut path, None, &mut out);
    out.walk_ms = walk_started.elapsed().as_millis() as u64;
    Ok(out)
}

/// Walk the cached tree, depth first, recording a path as we go.
unsafe fn flatten(
    element: &IUIAutomationElement,
    hwnd: isize,
    path: &mut Vec<usize>,
    container: Option<&str>,
    out: &mut Observation,
) {
    if out.elements.len() >= MAX_ELEMENTS {
        out.truncated = true;
        return;
    }

    let control_type = element.CachedControlType().map(|c| c.0).unwrap_or(0);
    let role = role_of(control_type);
    let label = element.CachedName().map(|n| n.to_string()).unwrap_or_default();

    // The root itself is the window, not a control in it.
    if !path.is_empty() {
        let bbox = element.CachedBoundingRectangle().ok().and_then(rect_to_bbox);
        out.elements.push(Found {
            id: format!("uia:{hwnd:x}:{}", join(path)),
            role: role.to_string(),
            label: label.clone(),
            value: cached_value(element).filter(|v| !v.is_empty()),
            enabled: element.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true),
            bbox,
            container: container.map(|c| c.to_string()),
            // A password field's contents never reach the journal: the
            // broker redacts on this flag, and it is cached like any other
            // property rather than asked for per element.
            sensitive: element.CachedIsPassword().map(|b| b.as_bool()).unwrap_or(false),
        });
    }

    // What children of this element will call their container.
    let own = container_name(role, &label);
    let next_container = own.as_deref().or(container);

    let Ok(children) = element.GetCachedChildren() else { return };
    let count = children.Length().unwrap_or(0);
    for i in 0..count {
        let Ok(child) = children.GetElement(i) else { continue };
        path.push(i as usize);
        flatten(&child, hwnd, path, next_container, out);
        path.pop();
        if out.truncated {
            return;
        }
    }
}

fn join(path: &[usize]) -> String {
    path.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(".")
}

/// Whether this element is the sort of thing a container name should come
/// from, and what to call it.
///
/// The point of a container is telling two identically labelled controls
/// apart -- "the Send button in the compose dialog" -- so an unnamed grouping
/// adds nothing and is skipped rather than reported as an empty one.
fn container_name(role: &str, label: &str) -> Option<String> {
    if label.trim().is_empty() {
        return None;
    }
    matches!(
        role,
        "dialog" | "window" | "pane" | "group" | "toolbar" | "menubar" | "tab" | "list"
            | "table" | "tree" | "datagrid"
    )
    .then(|| format!("{role}:{label}"))
}

// ---------------------------------------------------------------------------
// Reading the cache
// ---------------------------------------------------------------------------

/// The control's current text, from the cache.
///
/// There is no typed `CachedValue` on the element -- value belongs to a
/// pattern -- so this is the one property read through a VARIANT. It is what
/// the broker's read-back evidence compares against after typing, so it is
/// worth the twenty lines.
unsafe fn cached_value(element: &IUIAutomationElement) -> Option<String> {
    let v = element.GetCachedPropertyValue(UIA_ValueValuePropertyId).ok()?;
    variant_string(&v)
}

/// Named so the property-id import is honest about being used.
#[allow(dead_code)]
fn _property_ids(_: UIA_PROPERTY_ID) {}

fn rect_to_bbox(r: RECT) -> Option<[i32; 4]> {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    // A zero-area rectangle is a control with no place on screen; reporting
    // it would give the decision model somewhere to aim that is nowhere.
    (w > 0 && h > 0).then_some([r.left, r.top, w, h])
}

/// UIA control type to the role names the protocol uses.
///
/// The same vocabulary the DOM backend reports, so a decision model sees one
/// set of words whichever backend answered. Anything unmapped keeps its UIA
/// name in lower case rather than becoming "unknown": a role nobody has
/// mapped is still information.
fn role_of(control_type: i32) -> &'static str {
    match control_type {
        50000 => "button",
        50001 => "calendar",
        50002 => "checkbox",
        50003 => "combobox",
        50004 => "edit",
        50005 => "link",
        50006 => "image",
        50007 => "listitem",
        50008 => "list",
        50009 => "menu",
        50010 => "menubar",
        50011 => "menuitem",
        50012 => "progressbar",
        50013 => "radiobutton",
        50014 => "scrollbar",
        50015 => "slider",
        50016 => "spinner",
        50017 => "statusbar",
        50018 => "tab",
        50019 => "tabitem",
        50020 => "text",
        50021 => "toolbar",
        50022 => "tooltip",
        50023 => "tree",
        50024 => "treeitem",
        50025 => "custom",
        50026 => "group",
        50027 => "thumb",
        50028 => "datagrid",
        50029 => "dataitem",
        50030 => "document",
        50031 => "splitbutton",
        50032 => "window",
        50033 => "pane",
        50034 => "header",
        50035 => "headeritem",
        50036 => "table",
        50037 => "titlebar",
        50038 => "separator",
        50039 => "semanticzoom",
        50040 => "appbar",
        _ => "element",
    }
}

/// `IUIAutomationElementArray` is only used behind the cached walk; naming it
/// here keeps the import honest.
#[allow(dead_code)]
fn _uses(_: Option<IUIAutomationElementArray>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_use_the_same_words_as_the_dom_backend() {
        assert_eq!(role_of(50000), "button");
        assert_eq!(role_of(50004), "edit");
        assert_eq!(role_of(50005), "link");
        // Unmapped is not unknown.
        assert_eq!(role_of(99999), "element");
    }

    #[test]
    fn a_container_needs_a_name_and_a_grouping_role() {
        assert_eq!(container_name("dialog", "New Message").as_deref(), Some("dialog:New Message"));
        assert_eq!(container_name("pane", "Mail").as_deref(), Some("pane:Mail"));
        // A button is not a container, and an unnamed pane tells nobody
        // anything.
        assert!(container_name("button", "Send").is_none());
        assert!(container_name("pane", "   ").is_none());
    }

    #[test]
    fn a_zero_area_rectangle_is_not_somewhere_to_aim() {
        assert_eq!(
            rect_to_bbox(RECT { left: 10, top: 20, right: 110, bottom: 60 }),
            Some([10, 20, 100, 40])
        );
        assert!(rect_to_bbox(RECT { left: 5, top: 5, right: 5, bottom: 90 }).is_none());
        assert!(rect_to_bbox(RECT { left: 0, top: 0, right: 0, bottom: 0 }).is_none());
    }

    #[test]
    fn an_element_id_names_its_window_and_its_path() {
        assert_eq!(join(&[0, 3, 7]), "0.3.7");
        assert_eq!(join(&[]), "");
    }
}
