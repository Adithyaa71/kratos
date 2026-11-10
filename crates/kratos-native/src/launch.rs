//! Opening an application, and bringing a window to the front.
//!
//! The two kinds the accessibility tree cannot do anything with: there is no
//! element for "Notepad, which is not running", and no pattern that focuses a
//! window from outside it. The Python backend this replaces did both, so this
//! has to, or retiring it loses the ability to start anything.
//!
//! **Enumerate, do not guess.** The catalogue is the Start Menu, which is
//! what the machine actually has installed, plus a handful of system tools
//! that do not always get a shortcut. A hardcoded list of fourteen names is
//! how `open X` fails for the other hundred and forty.
//!
//! Nothing here invents a target. The broker passes the name the operator's
//! sentence carried, and an unresolvable name is reported as unresolvable
//! together with the closest things in the catalogue -- which is a refusal
//! with candidates, the same shape as every other ambiguity in this system.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long to wait for a window to appear after a launch.
///
/// A launch that has not drawn anything yet is not a failure, and reporting
/// one would make every cold start look broken. Six seconds is what a cold
/// Chromium takes on this machine; past that the caller is told no window
/// appeared, not that nothing happened.
const WINDOW_WAIT: Duration = Duration::from_secs(6);

/// `{lowercased name: what to run}`, built once.
///
/// Once because walking two Start Menu trees is tens of milliseconds and the
/// set changes when software is installed, not during a session. A backend
/// restart rebuilds it.
fn catalogue() -> &'static BTreeMap<String, PathBuf> {
    static CATALOGUE: OnceLock<BTreeMap<String, PathBuf>> = OnceLock::new();
    CATALOGUE.get_or_init(build)
}

fn build() -> BTreeMap<String, PathBuf> {
    let mut out: BTreeMap<String, PathBuf> = BTreeMap::new();

    let roots = [
        std::env::var_os("ProgramData").map(PathBuf::from),
        std::env::var_os("APPDATA").map(PathBuf::from),
    ];
    for root in roots.into_iter().flatten() {
        let start = root.join(r"Microsoft\Windows\Start Menu\Programs");
        walk(&start, 0, &mut out);
    }

    // The system tools that do not reliably have a shortcut. Named by the
    // words a person uses for them, not by their executable.
    let system32 =
        PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into()))
            .join("System32");
    for (exe, names) in [
        ("notepad.exe", &["notepad"][..]),
        ("calc.exe", &["calculator", "calc"][..]),
        ("mspaint.exe", &["paint"][..]),
        ("explorer.exe", &["file explorer", "explorer", "files"][..]),
        ("taskmgr.exe", &["task manager"][..]),
        ("control.exe", &["control panel"][..]),
    ] {
        let full = system32.join(exe);
        if full.is_file() {
            for name in names {
                out.entry((*name).to_string()).or_insert_with(|| full.clone());
            }
        }
    }
    out
}

/// Shortcuts, a few levels down. Start Menu trees are shallow and wide.
fn walk(dir: &std::path::Path, depth: usize, out: &mut BTreeMap<String, PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.entry(stem.trim().to_lowercase()).or_insert(path);
            }
        }
    }
}

/// How many applications this machine offers, for the record.
pub fn installed() -> usize {
    catalogue().len()
}

/// What to run for a name the operator used.
///
/// Exact first, then "starts with", then "contains". In that order because
/// "word" must not resolve to "WordPad" while an exact match exists, and
/// because the loosest rule is the one most likely to be wrong.
pub fn resolve(name: &str) -> Option<&'static PathBuf> {
    let want = name.trim().to_lowercase();
    if want.is_empty() {
        return None;
    }
    let cat = catalogue();
    if let Some(p) = cat.get(&want) {
        return Some(p);
    }
    if let Some((_, p)) = cat.iter().find(|(k, _)| k.starts_with(&want)) {
        return Some(p);
    }
    cat.iter().find(|(k, _)| k.contains(&want)).map(|(_, p)| p)
}

/// The names closest to one that did not resolve, so a refusal can say what
/// this machine does have.
pub fn near(name: &str, limit: usize) -> Vec<String> {
    let want = name.trim().to_lowercase();
    let first = want.chars().next();
    catalogue()
        .keys()
        .filter(|k| first.is_some_and(|c| k.starts_with(c)) || k.contains(&want))
        .take(limit)
        .cloned()
        .collect()
}

/// Whether a string is something to hand to the shell as a location.
///
/// A URL, or a path. Not a guess: `open "notepad"` must not become a web
/// search, and `open "example.com"` must not look for an installed program
/// called example.com.
pub fn looks_like_a_location(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("http://")
        || t.starts_with("https://")
        || t.starts_with(r"\\")
        || (t.len() > 2 && t.as_bytes()[1] == b':' && (t.as_bytes()[2] == b'\\' || t.as_bytes()[2] == b'/'))
}

#[derive(Debug)]
pub struct Started {
    /// The window that appeared, if one did within the wait. `None` is not a
    /// failure: a cold start that has not drawn yet has not failed, and the
    /// caller is told that no window appeared rather than that nothing ran.
    pub window: Option<String>,
}

/// Start something and wait for its window.
///
/// Through the shell, because the catalogue holds `.lnk` files and a shortcut
/// is not an executable: it carries the arguments and working directory the
/// installer chose, and running the target without them is how an app starts
/// and immediately exits.
pub fn start(target: &str) -> Result<Started, String> {
    let before = top_level_titles();

    // `ShellExecuteW`, which is exactly what double-clicking does.
    //
    // Not `Command::new(target)`: the catalogue holds `.lnk` files, and a
    // shortcut is not an executable -- CreateProcess cannot run one, and the
    // arguments and working directory the installer chose live inside it.
    // Not `explorer.exe <thing>` either, which was the first attempt here: it
    // started nothing at all for an .exe path, because explorer treats that
    // as something to show you rather than something to run.
    shell_execute(target)?;

    let deadline = Instant::now() + WINDOW_WAIT;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
        let now = top_level_titles();
        if let Some(new) = now.iter().find(|t| !before.contains(*t)) {
            return Ok(Started { window: Some(new.clone()) });
        }
    }
    Ok(Started { window: None })
}

/// Bring a window to the front by a substring of its title.
///
/// Returns the title it settled on, so the broker can journal what was
/// actually focused rather than what was asked for.
///
/// And it checks. `SetForegroundWindow` is refused, silently and by returning
/// false, when the calling process does not own the foreground -- and it is
/// refused outright while the session is locked. Reporting a focus that did
/// not happen would be a success the next step builds on: a keypress goes to
/// whatever has focus, so a wrong answer here types into the wrong window.
pub fn focus(title_substring: &str) -> Result<String, String> {
    let hwnd = crate::window::by_title(title_substring)
        .ok_or_else(|| format!("no visible window has {title_substring:?} in its title"))?;
    raise(hwnd);
    let title = crate::window::title(hwnd);
    if crate::window::find(None) == Some(hwnd) {
        Ok(title)
    } else {
        Err(format!(
            "{title:?} was found and asked to come forward, and Windows refused: the foreground              is {:?}. A locked session, or a window holding it open, does this",
            crate::window::find(None).map(crate::window::title).unwrap_or_default()
        ))
    }
}

/// Hand something to the shell the way a double-click does.
fn shell_execute(target: &str) -> Result<(), String> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    // The return is a fake HINSTANCE for compatibility: 32 or less is an
    // error code, anything above it means the shell took it.
    let rc = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if rc.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!("the shell refused {target:?} (code {})", rc.0 as usize))
    }
}

/// The windows a person could see, by title.
fn top_level_titles() -> Vec<String> {
    crate::window::visible_titles()
}

/// Make a window foreground, including from a process that does not own the
/// foreground.
///
/// `SetForegroundWindow` is refused when the caller is not the foreground
/// process, silently, returning false. Restoring a minimised window first and
/// then asking twice is the pattern that works without the
/// attach-thread-input trick; when it still fails the caller is told.
fn raise(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let h = HWND(hwnd as *mut _);
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = SetForegroundWindow(h);
        let _ = SetForegroundWindow(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_has_a_catalogue_and_it_is_not_a_guess_list() {
        // The point of enumerating: more than a hardcoded handful.
        assert!(installed() > 14, "only {} applications found", installed());
    }

    #[test]
    fn notepad_resolves_because_every_windows_machine_has_it() {
        let p = resolve("notepad").expect("notepad");
        assert!(
            p.to_string_lossy().to_lowercase().contains("notepad"),
            "{}",
            p.display()
        );
    }

    #[test]
    fn an_exact_name_beats_a_longer_one_that_starts_with_it() {
        // "notepad" must not resolve to "Notepad++" while Notepad exists.
        let p = resolve("notepad").expect("notepad");
        let name = p.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
        assert!(name == "notepad", "resolved to {name:?}");
    }

    #[test]
    fn nothing_resolves_from_nothing() {
        assert!(resolve("").is_none());
        assert!(resolve("   ").is_none());
        assert!(resolve("qzxvbnmlkjhgfdsa").is_none());
    }

    #[test]
    fn a_name_that_did_not_resolve_comes_back_with_candidates() {
        // A refusal that says what the machine does have is actionable; one
        // that says "no" is not.
        let near = near("note", 5);
        assert!(!near.is_empty());
        assert!(near.len() <= 5);
    }

    #[test]
    fn a_url_is_told_apart_from_an_application_name() {
        assert!(looks_like_a_location("https://example.com"));
        assert!(looks_like_a_location("http://example.com"));
        assert!(looks_like_a_location(r"C:\Windows\notepad.exe"));
        assert!(looks_like_a_location(r"\\server\share"));
        assert!(!looks_like_a_location("notepad"));
        assert!(!looks_like_a_location("example.com"));
    }
}
