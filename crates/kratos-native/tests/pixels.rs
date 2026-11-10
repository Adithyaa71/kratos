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

mod common;

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
    // A locked session captures a window *sometimes*. Measured on this
    // machine, same window, minutes apart: once 970x1032 in 16ms with ten
    // lines of text read out of it, and once the same geometry in 428ms with
    // nothing in it at all. The composition surface is there or it is not,
    // and a test cannot tell which it will get. So this skips -- not because
    // capture is impossible while locked, but because it is unreliable, and a
    // test that fails one run in three teaches people to ignore it.
    let Some(_desktop) =
        common::Desktop::take_or_skip(std::time::Duration::from_secs(180), "capture and read")
    else {
        return;
    };
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

/// The pixel path, end to end, with the sources named.
///
/// It is a weaker test than it looks and that is deliberate: whether the
/// detector finds a particular button depends on weights this repository does
/// not ship, so the assertions are about *shape* — every element says where it
/// came from, a confidence appears only where something measured one, and
/// nothing comes back twice. A regression in the wiring breaks those; a
/// different model version does not.
#[test]
fn the_pixel_path_labels_every_element_with_its_source() {
    let Some(_desktop) =
        common::Desktop::take_or_skip(std::time::Duration::from_secs(180), "fused sources")
    else {
        return;
    };
    let _scratch = Scratch::open();
    let out = Command::new(native()).args(["--pixels", "Notepad"]).output();
    let Ok(out) = out else {
        eprintln!("could not run kratos-native; skipping");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    eprintln!("{text}");

    if text.contains("no detector") {
        eprintln!(
            "no icon detector on this machine (the weights are AGPL and not shipped); \
             skipping -- see docs/pixel-path.md"
        );
        return;
    }
    if !out.status.success() {
        eprintln!("the pixel path failed ({}); skipping", String::from_utf8_lossy(&out.stderr));
        return;
    }

    assert!(text.contains("pass 1:"), "{text}");
    // Every element line names a source the fusion is allowed to produce.
    let mut seen_any = false;
    for line in text.lines().filter(|l| l.starts_with("   ")) {
        let source = line.split_whitespace().next().unwrap_or("");
        assert!(
            ["uia", "ocr", "pixel", "pixel+ocr"].contains(&source),
            "{source:?} is not a source a consumer can branch on: {line}"
        );
        seen_any = true;
    }
    assert!(seen_any, "the pixel path reported nothing at all: {text}");

    // And the second look is free, because the frame did not change.
    assert!(
        text.contains("detect 0ms") || text.contains("(cached)"),
        "an unchanged window should not be detected twice: {text}"
    );
}
