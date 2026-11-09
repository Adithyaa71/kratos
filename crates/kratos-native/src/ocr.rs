//! Reading the pixels, when nothing else can.
//!
//! `Windows.Media.Ocr` ships with Windows. It needs no install, no model
//! download and no network, and the languages it reads are the ones the user
//! already has installed — which is why it is the default here and Tesseract
//! is not. A perception backend that asks the operator to install a toolchain
//! before it can see a canvas app is a perception backend that does not run.
//!
//! It is not the most accurate OCR available, and the spec keeps a door open
//! for a better one behind a switch. This is the one that is always there.

use std::sync::Mutex;
use std::time::Instant;

use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrResult;
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncOperation};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;

use crate::capture::Frame;

/// One run of text, where it is, and how sure the engine was.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Line {
    pub text: String,
    /// `[x, y, w, h]` in physical screen pixels, already offset by the
    /// window's origin, so it is somewhere a mouse could go.
    pub bbox: [i32; 4],
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Reading {
    pub lines: Vec<Line>,
    pub took_ms: u64,
    /// True when this came back from the frame cache rather than the engine.
    pub cached: bool,
}

/// The OCR engine, and what it last read.
///
/// WinRT objects are not `Send`, and the engine is built once: creating it per
/// call costs more than the recognition. It lives behind a mutex on the thread
/// that owns it, like the UIA apartment.
pub struct Ocr {
    engine: OcrEngine,
    /// The last frame read, so a window that has not changed is not read
    /// twice. Most of what an agent looks at between two steps is identical.
    last: Mutex<Option<(u64, Vec<Line>)>>,
}

impl Ocr {
    pub fn new() -> Result<Ocr, String> {
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| format!("no OCR engine for this user's languages: {e}"))?;
        Ok(Ocr { engine, last: Mutex::new(None) })
    }

    /// The languages this machine can read, for diagnostics.
    pub fn languages() -> Vec<String> {
        OcrEngine::AvailableRecognizerLanguages()
            .map(|langs| {
                langs
                    .into_iter()
                    .filter_map(|l| l.LanguageTag().ok().map(|t| t.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Read a captured frame.
    pub fn read(&self, frame: &Frame) -> Result<Reading, String> {
        let started = Instant::now();
        let print = frame.fingerprint();

        if let Ok(last) = self.last.lock() {
            if let Some((seen, lines)) = last.as_ref() {
                if *seen == print {
                    return Ok(Reading {
                        lines: lines.clone(),
                        took_ms: started.elapsed().as_millis() as u64,
                        cached: true,
                    });
                }
            }
        }

        let bitmap = to_bitmap(frame)?;
        let operation =
            self.engine.RecognizeAsync(&bitmap).map_err(|e| format!("RecognizeAsync: {e}"))?;
        let result = wait_for(operation).map_err(|e| format!("OCR did not finish: {e}"))?;

        let mut lines = Vec::new();
        if let Ok(found) = result.Lines() {
            for line in found {
                let Ok(text) = line.Text() else { continue };
                let text = text.to_string();
                if text.trim().is_empty() {
                    continue;
                }
                // A line has no rectangle of its own; its words do, so the
                // line's box is their union.
                let Some(bbox) = line_bbox(&line, frame.origin) else { continue };
                lines.push(Line { text, bbox });
            }
        }

        if let Ok(mut last) = self.last.lock() {
            *last = Some((print, lines.clone()));
        }
        Ok(Reading { lines, took_ms: started.elapsed().as_millis() as u64, cached: false })
    }
}

fn line_bbox(
    line: &windows::Media::Ocr::OcrLine,
    origin: (i32, i32),
) -> Option<[i32; 4]> {
    let words = line.Words().ok()?;
    let (mut x0, mut y0) = (f64::MAX, f64::MAX);
    let (mut x1, mut y1) = (f64::MIN, f64::MIN);
    let mut any = false;
    for word in words {
        let Ok(r) = word.BoundingRect() else { continue };
        any = true;
        x0 = x0.min(r.X as f64);
        y0 = y0.min(r.Y as f64);
        x1 = x1.max((r.X + r.Width) as f64);
        y1 = y1.max((r.Y + r.Height) as f64);
    }
    if !any || x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some([
        origin.0 + x0.round() as i32,
        origin.1 + y0.round() as i32,
        (x1 - x0).round() as i32,
        (y1 - y0).round() as i32,
    ])
}

/// BGRA bytes to the `SoftwareBitmap` the engine wants.
///
/// Through a `DataWriter` rather than a raw buffer: the direct route needs
/// `IMemoryBufferByteAccess`, a COM interface the `windows` crate does not
/// surface, and this copy is a few hundred microseconds against a recognition
/// measured in tens of milliseconds.
fn to_bitmap(frame: &Frame) -> Result<SoftwareBitmap, String> {
    let writer = DataWriter::new().map_err(|e| format!("DataWriter: {e}"))?;
    writer.WriteBytes(&frame.bgra).map_err(|e| format!("WriteBytes: {e}"))?;
    let buffer = writer.DetachBuffer().map_err(|e| format!("DetachBuffer: {e}"))?;
    SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        frame.width,
        frame.height,
        // The capture path leaves alpha unset for some windows, and a bitmap
        // declared premultiplied with a zero alpha channel reads as entirely
        // transparent -- which OCR sees as a blank page.
        BitmapAlphaMode::Ignore,
    )
    .map_err(|e| format!("SoftwareBitmap: {e}"))
}

/// Block until a WinRT operation finishes.
///
/// `windows-future` has a `join` for exactly this and keeps it private in
/// this version, so the completion handler is wired by hand. That is all
/// `join` does: this thread owns no message loop, so there is nothing to pump
/// and nothing that can deadlock on waiting.
///
/// The timeout is not defensive decoration. OCR runs in a system service, and
/// a service that has wedged would otherwise hang the backend thread for
/// good -- and with it every observation of every window.
fn wait_for(operation: IAsyncOperation<OcrResult>) -> windows::core::Result<OcrResult> {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    operation.SetCompleted(&AsyncOperationCompletedHandler::new(move |_, status| {
        let _ = tx.send(status);
        Ok(())
    }))?;

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(AsyncStatus::Completed) => operation.GetResults(),
        Ok(other) => Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            format!("the OCR engine stopped: {other:?}"),
        )),
        Err(_) => Err(windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "the OCR engine did not answer within ten seconds",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_can_read_something() {
        let langs = Ocr::languages();
        eprintln!("OCR languages: {langs:?}");
        // Not an assertion about which: a machine with no language pack is a
        // real configuration, and the backend says so rather than failing to
        // start.
        match Ocr::new() {
            Ok(_) => assert!(!langs.is_empty(), "an engine exists but no languages are listed"),
            Err(e) => eprintln!("no OCR engine here ({e}); the pixel path will say so"),
        }
    }

    #[test]
    fn a_line_box_is_the_union_of_its_words_offset_by_the_window() {
        // The arithmetic without WinRT: a box found at (10,20) inside a window
        // whose top-left is (100,200) is at (110,220) on the desktop, because
        // what comes back has to be somewhere a mouse can go.
        let origin = (100, 200);
        let words = [(10.0f64, 20.0, 30.0, 8.0), (45.0, 19.0, 25.0, 10.0)];
        let x0 = words.iter().map(|w| w.0).fold(f64::MAX, f64::min);
        let y0 = words.iter().map(|w| w.1).fold(f64::MAX, f64::min);
        let x1 = words.iter().map(|w| w.0 + w.2).fold(f64::MIN, f64::max);
        let y1 = words.iter().map(|w| w.1 + w.3).fold(f64::MIN, f64::max);
        assert_eq!(
            [origin.0 + x0 as i32, origin.1 + y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32],
            [110, 219, 60, 10]
        );
    }
}
