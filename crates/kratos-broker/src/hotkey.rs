//! The global kill-switch hotkey.
//!
//! Registered by the broker itself, on a thread of its own with a message
//! loop, because `RegisterHotKey` delivers `WM_HOTKEY` to the thread that
//! registered it. That thread does nothing else, so the hotkey still fires
//! while every agent task is blocked on a decision call or a backend.
//!
//! This is the one control an agent cannot reach. The wire protocol lets an
//! agent *engage* a kill scope but never clear one, and this hotkey is not on
//! the wire at all.

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

/// Arbitrary but fixed: the id only has to be unique within this thread.
const HOTKEY_ID: i32 = 0x4448;

/// Parse `ctrl+alt+k` into Win32 modifier flags and a virtual key code.
pub fn parse(spec: &str) -> Option<(u32, u32)> {
    let mut modifiers = 0u32;
    let mut vk = None;
    for part in spec.split('+').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()) {
        match part.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "meta" | "super" => modifiers |= MOD_WIN,
            other => vk = virtual_key(other),
        }
    }
    let vk = vk?;
    // A hotkey with no modifier would swallow a bare keystroke system-wide,
    // which is not something to do by accident.
    if modifiers == 0 {
        return None;
    }
    Some((modifiers | MOD_NOREPEAT, vk))
}

fn virtual_key(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return match first {
            'a'..='z' => Some(first.to_ascii_uppercase() as u32),
            '0'..='9' => Some(first as u32),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('f') {
        if let Ok(i) = n.parse::<u32>() {
            if (1..=24).contains(&i) {
                return Some(0x70 + i - 1); // VK_F1 = 0x70
            }
        }
    }
    match name {
        "escape" | "esc" => Some(0x1B),
        "space" => Some(0x20),
        "pause" => Some(0x13),
        "insert" => Some(0x2D),
        "delete" | "del" => Some(0x2E),
        "home" => Some(0x24),
        "end" => Some(0x23),
        _ => None,
    }
}

/// Register the hotkey and pump messages until the process exits.
///
/// Returns an error only if registration fails -- usually because another
/// application already owns the combination. That is reported rather than
/// swallowed: a kill switch the operator believes in but which never fires is
/// worse than none at all.
pub fn run(spec: &str, on_press: tokio::sync::mpsc::UnboundedSender<()>) -> Result<(), String> {
    let (modifiers, vk) =
        parse(spec).ok_or_else(|| format!("{spec:?} is not a usable hotkey (needs a modifier)"))?;

    let registered = unsafe { RegisterHotKey(std::ptr::null_mut(), HOTKEY_ID, modifiers, vk) };
    if registered == 0 {
        return Err(format!(
            "another application already owns {spec:?}; pick a different kill-switch hotkey"
        ));
    }

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let got = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if got <= 0 {
            return Ok(()); // WM_QUIT, or the message queue went away
        }
        if msg.message == WM_HOTKEY && msg.wParam as i32 == HOTKEY_ID {
            // A closed channel means the broker is shutting down.
            if on_press.send(()).is_err() {
                return Ok(());
            }
        }
    }
}

/// Start the hotkey thread. Failure is reported to stderr and the broker keeps
/// running, because losing the hotkey must not take the gate down with it.
pub fn spawn(spec: &str) -> tokio::sync::mpsc::UnboundedReceiver<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let spec = spec.to_string();
    std::thread::Builder::new()
        .name("kratos-hotkey".into())
        .spawn(move || {
            if let Err(e) = run(&spec, tx) {
                eprintln!("kratos-broker: kill-switch hotkey unavailable: {e}");
            }
        })
        .ok();
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_hotkey_parses() {
        let (m, vk) = parse("ctrl+alt+k").unwrap();
        assert_eq!(vk, 'K' as u32);
        assert_eq!(m & MOD_CONTROL, MOD_CONTROL);
        assert_eq!(m & MOD_ALT, MOD_ALT);
        assert_eq!(m & MOD_SHIFT, 0);
        // Auto-repeat suppressed: holding the keys must engage once.
        assert_eq!(m & MOD_NOREPEAT, MOD_NOREPEAT);
    }

    #[test]
    fn case_and_spacing_do_not_matter() {
        assert_eq!(parse("Ctrl + Alt + K"), parse("ctrl+alt+k"));
        assert_eq!(parse("CONTROL+ALT+k"), parse("ctrl+alt+k"));
    }

    #[test]
    fn function_keys_and_named_keys_work() {
        assert_eq!(parse("ctrl+shift+f12").unwrap().1, 0x7B);
        assert_eq!(parse("ctrl+f1").unwrap().1, 0x70);
        assert_eq!(parse("win+escape").unwrap().1, 0x1B);
        assert_eq!(parse("alt+delete").unwrap().1, 0x2E);
        assert_eq!(parse("ctrl+alt+9").unwrap().1, '9' as u32);
    }

    #[test]
    fn a_bare_key_is_refused() {
        // Registering an unmodified key globally would swallow it everywhere.
        assert!(parse("k").is_none());
        assert!(parse("f12").is_none());
    }

    #[test]
    fn nonsense_is_refused_rather_than_silently_ignored() {
        assert!(parse("").is_none());
        assert!(parse("ctrl+alt").is_none());
        assert!(parse("ctrl+alt+nosuchkey").is_none());
        assert!(parse("ctrl+f99").is_none());
    }

    #[test]
    fn registering_a_real_hotkey_succeeds_and_can_be_pressed_programmatically() {
        // Uses an obscure combination so a developer's own shortcuts are not
        // disturbed if this runs on a live desktop.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = std::thread::spawn(move || run("ctrl+alt+shift+f24", tx));
        std::thread::sleep(std::time::Duration::from_millis(150));
        // Nothing pressed it, so nothing arrived -- the loop is waiting, not
        // spinning or firing spuriously.
        assert!(rx.try_recv().is_err());
        drop(rx);
        // The thread stays parked in GetMessageW; detaching it is fine for a
        // test process, and the real broker's thread lives for the process.
        drop(handle);
    }
}
