//! The mouse and the keyboard, when the control offers nothing better.
//!
//! Everything here is `SendInput`, which marks its events as injected — and
//! that is load-bearing in both directions. The broker's user-activity pause
//! reads the same flag to tell the operator's hand from the agent's, so an
//! agent that drove the machine any other way would pause itself after its
//! own first click.
//!
//! Text goes through the clipboard rather than key events whenever it is not
//! plain ASCII. A unicode character has no scan code, a dead-key layout turns
//! `'` into nothing until the next keystroke, and an emoji has no keys at
//! all. Pasting is one operation for any text in any layout, which is why it
//! is the default for anything the keyboard cannot spell.

use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2,
    VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

/// Between a press and the release that follows it.
///
/// Some applications sample the keyboard rather than reading messages, and a
/// press and release in the same millisecond is a keystroke they never see.
const HOLD: Duration = Duration::from_millis(12);

const CF_UNICODETEXT: u32 = 13;

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    data: i32,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                // Wheel delta, and nothing for the other events.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Absolute mouse coordinates are 0..65535 across the primary screen.
fn to_absolute(x: i32, y: i32) -> (i32, i32) {
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
    ((x * 65535) / w, (y * 65535) / h)
}

pub fn move_to(x: i32, y: i32) {
    let (ax, ay) = to_absolute(x, y);
    let mut input = mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, 0);
    input.Anonymous.mi.dx = ax;
    input.Anonymous.mi.dy = ay;
    send(&[input]);
}

pub fn click(x: i32, y: i32) {
    move_to(x, y);
    std::thread::sleep(HOLD);
    send(&[mouse(MOUSEEVENTF_LEFTDOWN, 0)]);
    std::thread::sleep(HOLD);
    send(&[mouse(MOUSEEVENTF_LEFTUP, 0)]);
}

pub fn scroll(ticks: i32) {
    // One notch is 120, and the sign is the direction a wheel turns: positive
    // is away from the user, which scrolls *up*. An agent asking to scroll by
    // a positive amount means down the page, so this inverts.
    send(&[mouse(MOUSEEVENTF_WHEEL, -ticks * 120)]);
}

fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_event(unit: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE
                    | if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Press a chord: modifiers down, key, modifiers up in reverse.
pub fn press(chord: &[VIRTUAL_KEY]) {
    let Some((main, modifiers)) = chord.split_last() else { return };
    for m in modifiers {
        send(&[key_event(*m, false)]);
    }
    std::thread::sleep(HOLD);
    send(&[key_event(*main, false)]);
    std::thread::sleep(HOLD);
    send(&[key_event(*main, true)]);
    for m in modifiers.iter().rev() {
        send(&[key_event(*m, true)]);
    }
}

/// `ctrl+shift+n`, `Enter`, `F5` to the keys to hold and the key to press.
///
/// The modifiers come first and the key last, which is the order [`press`]
/// expects. An unknown name returns `None` rather than a guess: pressing the
/// wrong key is worse than refusing.
pub fn key_chord(name: &str) -> Option<Vec<VIRTUAL_KEY>> {
    let mut keys = Vec::new();
    let lower = name.trim().to_ascii_lowercase();
    let parts: Vec<&str> = lower.split(['+', '-']).map(str::trim).filter(|p| !p.is_empty()).collect();
    let (last, modifiers) = parts.split_last()?;

    for m in modifiers {
        keys.push(match *m {
            "ctrl" | "control" => VK_CONTROL,
            "shift" => VK_SHIFT,
            "alt" => VK_MENU,
            "win" | "super" | "meta" => VK_LWIN,
            _ => return None,
        });
    }

    let main = match *last {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "space" | "spacebar" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        one if one.chars().count() == 1 => {
            let c = one.chars().next()?.to_ascii_uppercase();
            if !c.is_ascii_alphanumeric() {
                return None;
            }
            VIRTUAL_KEY(c as u16)
        }
        _ => return None,
    };
    keys.push(main);
    Some(keys)
}

/// Whether this text can be typed as keystrokes at all.
fn is_plain(text: &str) -> bool {
    text.chars().all(|c| c.is_ascii() && !c.is_control() || c == '\n' || c == '\t')
}

pub fn how_typed(text: &str) -> &'static str {
    if is_plain(text) {
        "keystrokes"
    } else {
        "clipboard paste"
    }
}

/// Type text at whatever has focus.
pub fn type_text(text: &str) {
    // Falling through when the paste fails is deliberate: another process can
    // hold the clipboard open, and typing what can be typed beats silently
    // doing nothing.
    if !is_plain(text) && paste(text) {
        return;
    }
    for ch in text.chars() {
        match ch {
            '\n' => press(&[VK_RETURN]),
            '\t' => press(&[VK_TAB]),
            _ => {
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    send(&[unicode_event(*unit, false)]);
                    send(&[unicode_event(*unit, true)]);
                }
            }
        }
    }
}

/// Put text on the clipboard and press Ctrl+V.
///
/// Returns whether it worked. The clipboard is a shared, single-owner
/// resource: another process can have it open, and `OpenClipboard` simply
/// fails while it does.
fn paste(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;

    unsafe {
        if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
            return false;
        }
        let ok = (|| -> bool {
            if EmptyClipboard().is_err() {
                return false;
            }
            let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, bytes) else { return false };
            let locked = GlobalLock(handle);
            if locked.is_null() {
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), locked as *mut u16, wide.len());
            let _ = GlobalUnlock(handle);
            // Ownership passes to the clipboard on success, so the memory is
            // not freed here -- and must not be.
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0))).is_ok()
        })();
        let _ = CloseClipboard();
        if !ok {
            return false;
        }
    }

    press(&[VK_CONTROL, VIRTUAL_KEY(b'V' as u16)]);
    true
}

/// What is on the clipboard now.
///
/// Used by the test that proves text the keyboard cannot spell survives the
/// round trip, and worth keeping for the same reason the host got a log: a
/// paste that silently pasted the wrong thing is hard to see from the far
/// end.
#[allow(dead_code)]
pub fn clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
            return None;
        }
        let out = (|| -> Option<String> {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let locked = GlobalLock(HGLOBAL(handle.0));
            if locked.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *(locked as *const u16).add(len) != 0 && len < 1 << 20 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(locked as *const u16, len);
            let text = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            Some(text)
        })();
        let _ = CloseClipboard();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chord_is_modifiers_then_the_key() {
        let chord = key_chord("ctrl+shift+n").expect("a chord");
        assert_eq!(chord.len(), 3);
        assert_eq!(chord[0], VK_CONTROL);
        assert_eq!(chord[1], VK_SHIFT);
        assert_eq!(chord[2], VIRTUAL_KEY(b'N' as u16));

        assert_eq!(key_chord("Enter"), Some(vec![VK_RETURN]));
        assert_eq!(key_chord(" f5 "), Some(vec![VK_F5]));
    }

    #[test]
    fn a_key_nobody_recognises_is_refused_rather_than_guessed() {
        // Pressing the wrong key is worse than pressing none.
        assert!(key_chord("the any key").is_none());
        assert!(key_chord("ctrl+").is_none());
        assert!(key_chord("hyper+a").is_none());
        assert!(key_chord("").is_none());
    }

    #[test]
    fn text_the_keyboard_cannot_spell_goes_through_the_clipboard() {
        assert_eq!(how_typed("hello@example.com"), "keystrokes");
        assert_eq!(how_typed("line\nbreak"), "keystrokes");
        // A dead-key layout, an emoji, a non-Latin script: none of these have
        // a scan code to send.
        assert_eq!(how_typed("naïve"), "clipboard paste");
        assert_eq!(how_typed("ok 👍"), "clipboard paste");
        assert_eq!(how_typed("こんにちは"), "clipboard paste");
    }

    #[test]
    fn the_wheel_turns_the_way_a_page_scrolls() {
        // Positive means down the page, which is a negative wheel delta.
        // Getting this backwards is a bug nobody notices until an agent
        // scrolls away from what it was looking for.
        let down = -3 * 120;
        assert_eq!(down, -360);
    }

    #[test]
    fn absolute_coordinates_span_the_screen() {
        let (x, y) = to_absolute(0, 0);
        assert_eq!((x, y), (0, 0));
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        if w > 0 {
            let (right, _) = to_absolute(w, 0);
            assert!(right >= 65000, "the right edge should be near 65535, got {right}");
        }
    }

    #[test]
    fn the_clipboard_round_trips_what_the_keyboard_cannot_type() {
        // Uses the real clipboard, and puts back what was there: a test that
        // eats the operator's clipboard is a test that gets disabled.
        let before = clipboard_text();
        let sample = "naïve 👍 こんにちは";
        // `paste` also presses Ctrl+V, which would type into whatever has
        // focus, so only the clipboard half is exercised here.
        let wide: Vec<u16> = sample.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
                eprintln!("the clipboard is held by another process; skipping");
                return;
            }
            let _ = EmptyClipboard();
            let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) else {
                let _ = CloseClipboard();
                return;
            };
            let locked = GlobalLock(handle);
            std::ptr::copy_nonoverlapping(wide.as_ptr(), locked as *mut u16, wide.len());
            let _ = GlobalUnlock(handle);
            let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0)));
            let _ = CloseClipboard();
        }
        assert_eq!(clipboard_text().as_deref(), Some(sample));

        if let Some(before) = before {
            let wide: Vec<u16> = before.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_ok() {
                    let _ = EmptyClipboard();
                    if let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
                        let locked = GlobalLock(handle);
                        std::ptr::copy_nonoverlapping(
                            wide.as_ptr(),
                            locked as *mut u16,
                            wide.len(),
                        );
                        let _ = GlobalUnlock(handle);
                        let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0)));
                    }
                    let _ = CloseClipboard();
                }
            }
        }
    }
}
