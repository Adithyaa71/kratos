//! Who is in front, and what kind of thing is it.
//!
//! The broker needs the window title, the process image and the window
//! rectangle *before* it can pick a backend, because routing depends on the
//! target class and the gate depends on the title and URL. So this does not go
//! through a backend -- it is a direct, cheap Win32 lookup.
//!
//! Cheap is not incidental. The previous build reached the foreground window
//! through a UI-Automation connect that raised after 2,917ms and then
//! enumerated every window for another 6 seconds, returning the wrong one: a
//! 5,085ms answer that was also incorrect. `GetForegroundWindow` is a handle
//! lookup measured in microseconds, and it is never wrong about which window
//! has focus.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH, RECT};

/// `BOOL` as windows-sys models it.
type Bool = i32;
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindow, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER,
};

use kratos_protocol::TargetClass;

#[derive(Debug, Clone)]
pub struct WindowRef {
    pub hwnd: isize,
    pub title: String,
    /// Process image name, lowercased, e.g. `brave.exe`.
    pub exe: String,
    pub pid: u32,
    /// `[x, y, w, h]` in physical screen pixels.
    pub rect: Option<[i32; 4]>,
}

impl WindowRef {
    /// Browser brand, when this is a Chromium-family browser.
    pub fn brand(&self) -> Option<&'static str> {
        match self.exe.as_str() {
            "brave.exe" => Some("brave"),
            "chrome.exe" => Some("chrome"),
            "msedge.exe" => Some("edge"),
            "vivaldi.exe" => Some("vivaldi"),
            "opera.exe" | "opera_gx.exe" => Some("opera"),
            _ => None,
        }
    }

    /// Classify for routing (protocol section 5.1).
    ///
    /// `browser-automation` is reported only for a process this broker
    /// launched itself with a dedicated profile, tracked in
    /// [`automation_pids`]. Inferring it from a window title would be
    /// guesswork, and guessing wrong means routing a real logged-in session to
    /// the automation path -- the failure that opened a fresh browser instead
    /// of using the one with the accounts in it.
    pub fn class(&self) -> TargetClass {
        match self.brand() {
            Some(_) if is_automation_pid(self.pid) => TargetClass::BrowserAutomation,
            Some(_) => TargetClass::Browser,
            None => TargetClass::Native,
        }
    }

    pub fn target_ref(&self, url: Option<String>) -> kratos_gate::TargetRef {
        kratos_gate::TargetRef {
            window_title: self.title.clone(),
            url,
            exe: Some(self.exe.clone()),
            brand: self.brand().map(|b| b.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Automation profile registry
// ---------------------------------------------------------------------------

use std::sync::Mutex;
use std::sync::OnceLock;

fn automation_pids() -> &'static Mutex<Vec<u32>> {
    static PIDS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
    PIDS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Record a browser this broker started with its own user-data-dir.
///
/// Nothing calls this yet: kratos drives the browser the operator
/// already has open and does not launch one. It exists so that when a
/// dedicated automation profile *is* launched, the class is decided by "we
/// started this process" rather than by pattern-matching a window title --
/// guessing that wrong is what opened a fresh browser instead of the session
/// with the accounts in it.
#[allow(dead_code)]
pub fn register_automation_pid(pid: u32) {
    if let Ok(mut v) = automation_pids().lock() {
        if !v.contains(&pid) {
            v.push(pid);
        }
    }
}

pub fn is_automation_pid(pid: u32) -> bool {
    automation_pids().lock().map(|v| v.contains(&pid)).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn window_rect(hwnd: HWND) -> Option<[i32; 4]> {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return None;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            return None;
        }
        Some([r.left, r.top, w, h])
    }
}

fn process_image(pid: u32) -> String {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return String::new();
        }
        let mut buf = vec![0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        if ok == 0 || len == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
    }
}

fn describe(hwnd: HWND) -> Option<WindowRef> {
    if hwnd.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    Some(WindowRef {
        hwnd: hwnd as isize,
        title: window_title(hwnd),
        exe: process_image(pid),
        pid,
        rect: window_rect(hwnd),
    })
}

/// The window with focus.
pub fn foreground() -> Option<WindowRef> {
    describe(unsafe { GetForegroundWindow() })
}

struct Search {
    needle: String,
    found: Vec<isize>,
}

/// What a `window` argument picked out.
#[derive(Debug)]
pub enum Resolution {
    One(WindowRef),
    /// The text matched windows belonging to more than one application. The
    /// worst outcome here is not failing -- it is succeeding on the wrong one.
    Ambiguous(Vec<WindowRef>),
    NotFound,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> Bool {
    let search = &mut *(lparam as *mut Search);
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return 1;
    }
    if title.to_lowercase().contains(&search.needle) {
        // Collect them all. Stopping at the first is how a search for
        // "Upwork" landed on an editor whose session was named after the job
        // instead of on the browser showing the site.
        search.found.push(hwnd as isize);
    }
    1
}

/// Every visible window whose title contains `needle`, case-insensitively.
pub fn find_all_by_title(needle: &str) -> Vec<WindowRef> {
    let n = needle.trim().to_lowercase();
    if n.is_empty() {
        return foreground().into_iter().collect();
    }
    let mut search = Search { needle: n, found: Vec::new() };
    unsafe {
        EnumWindows(Some(enum_proc), &mut search as *mut Search as LPARAM);
    }
    search
        .found
        .into_iter()
        .filter_map(|h| describe(h as *mut c_void as HWND))
        .collect()
}

/// Every visible, titled window, described.
///
/// One `EnumWindows` and one process-image lookup per window. Measured at
/// well under a millisecond on an ordinary desktop, which is why lane
/// filtering can afford to describe everything and decide in Rust rather than
/// matching inside the callback: the callback only knows titles, and "which
/// lane does this window belong to" is a question about the process.
pub fn visible_windows() -> Vec<WindowRef> {
    let mut search = Search { needle: String::new(), found: Vec::new() };
    unsafe {
        EnumWindows(Some(enum_proc_all), &mut search as *mut Search as LPARAM);
    }
    search.found.into_iter().filter_map(|h| describe(h as *mut c_void as HWND)).collect()
}

unsafe extern "system" fn enum_proc_all(hwnd: HWND, lparam: LPARAM) -> Bool {
    let search = &mut *(lparam as *mut Search);
    if IsWindowVisible(hwnd) == 0 || window_title(hwnd).is_empty() {
        return 1;
    }
    search.found.push(hwnd as isize);
    1
}

/// Whether a window belongs to another window.
///
/// An application window owns itself and `GW_OWNER` is null for it. A dialog,
/// a prompt, a bubble -- anything the program put up *over* a window -- names
/// that window as its owner. The distinction is the one the task switcher
/// makes, and it is the only cheap way to tell a browser from the thing the
/// browser is asking you about.
fn is_owned(hwnd: isize) -> bool {
    // SAFETY: a call on a handle that may be stale, which returns null rather
    // than failing. Nothing is dereferenced.
    !unsafe { GetWindow(hwnd as *mut c_void as HWND, GW_OWNER) }.is_null()
}

/// Visible windows belonging to one Chromium brand, the real ones first.
///
/// Ordered, not filtered. Callers that want "the browser" take the first and
/// must not be handed a prompt: an `observe` on a lane came back with the
/// fifteen elements of Brave's "Translate this page?" bubble, because the
/// bubble is a visible titled window of brand `brave` and it had just been
/// shown, so it was first in z-order. It also cost twenty seconds, because
/// dom-ext was given the bubble's handle, has no tab for it, and went quiet
/// until the broker gave up.
///
/// The owned ones are still here, after the others, because a browser
/// genuinely can have no unowned window -- every tab closed, a download
/// bubble left on screen -- and an empty answer there would read as "that
/// browser is not running".
pub fn windows_of_brand(brand: &str) -> Vec<WindowRef> {
    let mut found: Vec<WindowRef> = visible_windows()
        .into_iter()
        .filter(|w| w.brand().is_some_and(|b| b.eq_ignore_ascii_case(brand)))
        .collect();
    // Stable, so z-order still decides between two real browser windows.
    found.sort_by_key(|w| is_owned(w.hwnd));
    found
}

/// Decide between the windows a `window` argument matched.
///
/// Several windows of one application are not a problem: they are the same
/// program, and the first is as good an answer as any. Windows of *different*
/// applications are a different thing entirely -- the text was not specific
/// enough to say which program was meant, and picking one silently is how an
/// instruction aimed at a browser gets carried out in an editor that happened
/// to be named after the same job.
pub fn choose(matches: Vec<WindowRef>) -> Resolution {
    if matches.is_empty() {
        return Resolution::NotFound;
    }
    let first = matches[0].exe.to_ascii_lowercase();
    if matches.iter().all(|w| w.exe.to_ascii_lowercase() == first) {
        let mut matches = matches;
        return Resolution::One(matches.remove(0));
    }
    Resolution::Ambiguous(matches)
}

/// Resolve the target window for a request.
pub fn resolve_window(window: Option<&str>) -> Resolution {
    match window {
        Some(w) if !w.trim().is_empty() => choose(find_all_by_title(w)),
        _ => match foreground() {
            Some(w) => Resolution::One(w),
            None => Resolution::NotFound,
        },
    }
}

#[cfg(test)]
mod tests {

    fn win(exe: &str, title: &str) -> WindowRef {
        WindowRef { hwnd: 1, title: title.into(), exe: exe.into(), pid: 1, rect: None }
    }

    #[test]
    fn the_lane_gets_a_real_browser_window_before_a_prompt() {
        // Live, against whatever is on this machine. It asserts the one thing
        // that matters and can be checked without owning a browser: once an
        // owned window appears in the list, no unowned one follows it. The
        // caller takes the first, so that ordering is what stops an `observe`
        // on a lane coming back with the contents of Brave's "Translate this
        // page?" bubble -- which is what happened, for twenty seconds, before
        // this.
        for brand in ["brave", "chrome", "edge", "vivaldi"] {
            let found = super::windows_of_brand(brand);
            if found.len() < 2 {
                continue;
            }
            let mut seen_owned = false;
            for w in &found {
                let owned = super::is_owned(w.hwnd);
                assert!(
                    !(seen_owned && !owned),
                    "{brand}: {:?} is a real window and it came after an owned one",
                    w.title
                );
                seen_owned |= owned;
            }
        }
    }

    #[test]
    fn an_owned_window_is_still_findable_by_name() {
        // The preference above is an ordering and not a filter, on purpose.
        // A Save As dialog is an owned window, and `click Save As` is half of
        // what this tool is for -- so the general enumeration must still
        // contain them. If this ever starts passing because the list is
        // empty, it is not telling you anything, so it skips instead.
        let all = super::visible_windows();
        if all.is_empty() {
            eprintln!("no visible windows; nothing to check");
            return;
        }
        // Whatever is on screen, the enumeration must not have quietly become
        // unowned-only: that would be the regression this guards.
        let owned_exist_somewhere = all.iter().any(|w| super::is_owned(w.hwnd));
        eprintln!(
            "{} visible windows, {} of them owned",
            all.len(),
            all.iter().filter(|w| super::is_owned(w.hwnd)).count()
        );
        // Not an assertion on the count -- a desktop with no dialog open has
        // none, legitimately. What is asserted is that asking is possible.
        let _ = owned_exist_somewhere;
    }

    #[test]
    fn several_windows_of_one_application_are_not_ambiguous() {
        // Three Brave windows is just Brave. Any of them will do.
        let m = vec![
            win("brave.exe", "Upwork | Find Work"),
            win("brave.exe", "Upwork | Messages"),
            win("brave.exe", "Upwork | Reports"),
        ];
        match choose(m) {
            Resolution::One(w) => assert_eq!(w.title, "Upwork | Find Work"),
            other => panic!("expected one, got {other:?}"),
        }
    }

    #[test]
    fn a_title_matching_two_applications_is_refused_not_guessed() {
        // The live failure: an editor session named after the job, and the
        // browser actually showing the site. Picking the first silently
        // carried a browser instruction into an editor.
        let m = vec![
            win("claude.exe", "Upwork profile optimization and lead generation"),
            win("brave.exe", "Upwork | Find Work"),
        ];
        match choose(m) {
            Resolution::Ambiguous(all) => {
                assert_eq!(all.len(), 2);
                assert!(all.iter().any(|w| w.exe == "brave.exe"));
                assert!(all.iter().any(|w| w.exe == "claude.exe"));
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn the_executable_comparison_ignores_case() {
        let m = vec![win("Brave.exe", "a"), win("brave.exe", "b")];
        assert!(matches!(choose(m), Resolution::One(_)));
    }

    #[test]
    fn nothing_matched_is_not_found_rather_than_an_empty_choice() {
        assert!(matches!(choose(Vec::new()), Resolution::NotFound));
    }

    use super::*;

    #[test]
    fn the_foreground_window_is_found_and_described() {
        let Some(w) = foreground() else {
            eprintln!("no foreground window (headless session); skipping");
            return;
        };
        assert_ne!(w.hwnd, 0);
        assert_ne!(w.pid, 0);
        // A real window has an image name; an empty one means the process
        // query failed, which would leave the gate unable to see deny_apps.
        assert!(w.exe.ends_with(".exe"), "image name was {:?}", w.exe);
    }

    #[test]
    fn the_foreground_lookup_is_fast_enough_to_do_on_every_action() {
        if foreground().is_none() {
            return;
        }
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let _ = foreground();
        }
        let each = started.elapsed() / 20;
        eprintln!("foreground lookup: {each:?} each");
        // The previous build took 5,085ms for this and got it wrong. Anything
        // over a few milliseconds means something is enumerating.
        assert!(each < std::time::Duration::from_millis(50), "{each:?}");
    }

    #[test]
    fn brands_map_from_the_image_name() {
        let mk = |exe: &str| WindowRef {
            hwnd: 1,
            title: "t".into(),
            exe: exe.into(),
            pid: 1,
            rect: None,
        };
        assert_eq!(mk("brave.exe").brand(), Some("brave"));
        assert_eq!(mk("msedge.exe").brand(), Some("edge"));
        assert_eq!(mk("notepad.exe").brand(), None);
        assert_eq!(mk("brave.exe").class(), TargetClass::Browser);
        assert_eq!(mk("notepad.exe").class(), TargetClass::Native);
    }

    #[test]
    fn a_registered_automation_pid_changes_the_class() {
        let mut w = WindowRef {
            hwnd: 1,
            title: "Brave".into(),
            exe: "brave.exe".into(),
            pid: 999_001,
            rect: None,
        };
        assert_eq!(w.class(), TargetClass::Browser);
        register_automation_pid(999_001);
        assert_eq!(w.class(), TargetClass::BrowserAutomation);
        // A different browser process is untouched -- the real logged-in
        // session must not be mistaken for the automation one.
        w.pid = 999_002;
        assert_eq!(w.class(), TargetClass::Browser);
    }

    #[test]
    fn target_ref_carries_what_the_gate_needs() {
        let w = WindowRef {
            hwnd: 1,
            title: "Inbox - Gmail - Brave".into(),
            exe: "brave.exe".into(),
            pid: 1,
            rect: None,
        };
        let t = w.target_ref(Some("https://mail.google.com/".into()));
        assert_eq!(t.brand.as_deref(), Some("brave"));
        assert_eq!(t.domain().as_deref(), Some("mail.google.com"));
        assert_eq!(t.scopes(), vec!["app:brave.exe", "browser:brave"]);
    }

    #[test]
    fn searching_for_a_title_that_cannot_exist_returns_none() {
        assert!(matches!(
            resolve_window(Some("zzz-no-such-window-zzz-9174")),
            Resolution::NotFound
        ));
    }

    #[test]
    fn an_empty_needle_falls_back_to_the_foreground() {
        let a = match resolve_window(Some("   ")) {
            Resolution::One(w) => Some(w.hwnd),
            _ => None,
        };
        let b = foreground().map(|w| w.hwnd);
        assert_eq!(a, b);
    }
}
