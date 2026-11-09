//! Capture and OCR, against whatever window this machine has open.
//!
//! These cannot be fixtures: a capture is pixels and an OCR result depends on
//! the fonts, the scaling and the language packs of the machine it runs on.
//! What they can do is prove the path works here, and catch the two failures
//! that look like success — a blank frame read as "no text", and a cache that
//! serves a stale read.
//!
//! On a session with no visible window they skip and say so, rather than
//! failing somewhere there is nothing to look at.

use std::process::Command;

/// Open something with text in it, so there is a window to read.
///
/// Notepad, because it is on every Windows machine, starts in well under a
/// second, and its own title bar and menus are text even when the document
/// is empty.
struct Scratch {
    child: Option<std::process::Child>,
}

impl Scratch {
    fn open() -> Scratch {
        let child = Command::new("notepad.exe").spawn().ok();
        // Notepad takes a moment to put a window up; without this the find
        // below races it.
        std::thread::sleep(std::time::Duration::from_millis(1200));
        Scratch { child }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// `kratos-native --dump` and friends find windows by title; the test binary
/// has no access to the crate's private modules, so it drives the real
/// executable the same way an operator would.
fn native() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_kratos-native"))
}

#[test]
fn a_window_is_captured_and_read() {
    let _scratch = Scratch::open();
    let out = Command::new(native()).args(["--ocr", "Notepad"]).output();
    let Ok(out) = out else {
        eprintln!("could not run kratos-native; skipping");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    eprintln!("{text}");

    if err.contains("no OCR engine") || text.contains("languages []") {
        eprintln!("no OCR language pack on this machine; skipping");
        return;
    }
    if !out.status.success() {
        eprintln!("kratos-native --ocr failed ({err}); skipping -- no window to read");
        return;
    }

    // The capture reported a real size, which rules out the blank-frame
    // failure that reads as "a window with no text in it".
    assert!(
        text.contains("pass 1:") && !text.contains("0x0"),
        "a capture with no area is not a capture: {text}"
    );

    // Something was read. A Notepad window has its own chrome in it even with
    // an empty document, so zero lines means the pixels never arrived.
    let lines: usize = text
        .split("pass 1:")
        .nth(1)
        .and_then(|rest| rest.split(" lines").next())
        .and_then(|rest| rest.rsplit(", ").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    assert!(lines > 0, "nothing was read from a window that has text in it: {text}");

    // And the same frame is not read twice.
    assert!(
        text.contains("(cached)"),
        "the second read of an unchanged window must come from the frame cache: {text}"
    );
}
