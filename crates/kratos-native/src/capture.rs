//! One window's pixels.
//!
//! `kratos-shot` grabs the whole virtual desktop and crops, which costs 33ms
//! whatever you wanted out of it and is the right trade when what you want is
//! two rectangles from the same instant. Reading *one window* is a different
//! question, and `PrintWindow` answers it directly: the window renders itself
//! into a bitmap, so the answer is right even when something is sitting on top
//! of it — which a screen grab cannot say.
//!
//! The fallback matters as much as the main path. `PrintWindow` returns an
//! empty frame for some hardware-composited windows (a browser's page content
//! being the one that counts here), and an all-black bitmap handed to OCR is
//! not a failure anybody notices — it is a window with no text in it. So the
//! result is checked for being blank, and a blank one falls back to copying
//! the window's rectangle off the screen.

use std::time::Instant;

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    SRCCOPY,
};
// `PrintWindow` lives under Storage::Xps, which is where the printing API
// it was originally part of ended up. It is a user32 function like any other.
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

/// Render the window's own content, including anything drawn by the GPU.
///
/// Undocumented in the public headers for years and now standard: without it,
/// `PrintWindow` on a Chromium window returns the frame and no page.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);

pub struct Frame {
    pub width: i32,
    pub height: i32,
    /// Top-left of the window on the virtual desktop, so a box found in this
    /// frame can be turned into something clickable.
    pub origin: (i32, i32),
    /// BGRA, top-down, `width * height * 4` bytes.
    pub bgra: Vec<u8>,
    /// How the pixels were obtained, because the two are not equally
    /// trustworthy and a caller debugging a blank OCR deserves to know.
    pub how: &'static str,
    pub took_ms: u64,
}

impl Frame {
    /// A cheap content hash, for "has anything changed since last time".
    ///
    /// FNV-1a over every 16th pixel: a full hash of a 1920x1080 frame is 8MB
    /// of reading to answer a question that is almost always "no", and
    /// sampling catches anything that moved a glyph.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for chunk in self.bgra.chunks_exact(64) {
            for b in &chunk[..4] {
                h ^= *b as u64;
                h = h.wrapping_mul(0x1000_0000_01b3);
            }
        }
        h ^= (self.width as u64) << 32 | self.height as u64;
        h
    }

    pub fn is_blank(&self) -> bool {
        // Every sampled pixel identical means nothing was drawn: a black
        // bitmap from a PrintWindow that silently did nothing looks exactly
        // like a window with no content, and OCR would report "no text".
        let mut iter = self.bgra.chunks_exact(4).step_by(37);
        let Some(first) = iter.next() else { return true };
        iter.all(|p| p[..3] == first[..3])
    }
}

/// Capture one window.
pub fn window(hwnd: isize) -> Result<Frame, String> {
    let started = Instant::now();
    let h = HWND(hwnd as *mut _);
    let mut rect = RECT::default();
    unsafe { GetWindowRect(h, &mut rect) }
        .map_err(|e| format!("{}: GetWindowRect: {e}", crate::uia::GONE))?;
    let (w, ht) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || ht <= 0 {
        return Err("that window has no area on screen".to_string());
    }

    let mut frame = grab(h, rect, w, ht, true)?;
    if frame.is_blank() {
        // Hardware-composited content: the window rendered nothing into our
        // bitmap. What is on the screen is the next best answer, and it is
        // only wrong where something is covering the window.
        let second = grab(h, rect, w, ht, false)?;
        if !second.is_blank() {
            frame = second;
        }
    }
    frame.took_ms = started.elapsed().as_millis() as u64;
    Ok(frame)
}

unsafe fn with_dcs<T>(
    body: impl FnOnce(HDC, HDC, HBITMAP) -> Result<T, String>,
    w: i32,
    h: i32,
) -> Result<T, String> {
    let screen = GetDC(None);
    if screen.is_invalid() {
        return Err("no screen device context".to_string());
    }
    let mem = CreateCompatibleDC(Some(screen));
    let bitmap = CreateCompatibleBitmap(screen, w, h);
    let previous = SelectObject(mem, bitmap.into());

    let out = body(screen, mem, bitmap);

    SelectObject(mem, previous);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    out
}

fn grab(h: HWND, rect: RECT, w: i32, ht: i32, print: bool) -> Result<Frame, String> {
    unsafe {
        with_dcs(
            |screen, mem, bitmap| {
                if print {
                    // Zero is a failure, and so is a frame full of one colour;
                    // both are handled by the caller rather than here, because
                    // only the caller knows whether a second attempt is worth
                    // it.
                    let _ = PrintWindow(h, mem, PW_RENDERFULLCONTENT);
                } else {
                    BitBlt(mem, 0, 0, w, ht, Some(screen), rect.left, rect.top, SRCCOPY)
                        .map_err(|e| format!("BitBlt: {e}"))?;
                }

                let mut info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        // Negative: top-down, so row 0 is the top of the
                        // window and a bounding box needs no flipping.
                        biHeight: -ht,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bgra = vec![0u8; (w * ht * 4) as usize];
                let copied = GetDIBits(
                    mem,
                    bitmap,
                    0,
                    ht as u32,
                    Some(bgra.as_mut_ptr() as *mut _),
                    &mut info,
                    DIB_RGB_COLORS,
                );
                if copied == 0 {
                    return Err("GetDIBits copied nothing".to_string());
                }
                Ok(Frame {
                    width: w,
                    height: ht,
                    origin: (rect.left, rect.top),
                    bgra,
                    how: if print { "printwindow" } else { "screen" },
                    took_ms: 0,
                })
            },
            w,
            ht,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bgra: Vec<u8>, w: i32, h: i32) -> Frame {
        Frame { width: w, height: h, origin: (0, 0), bgra, how: "test", took_ms: 0 }
    }

    #[test]
    fn a_frame_of_one_colour_is_blank() {
        // The case that matters: PrintWindow returning an untouched bitmap
        // for a GPU-composited window, which reads as "no text on screen"
        // rather than as a failure.
        let black = frame(vec![0u8; 400 * 4], 20, 20);
        assert!(black.is_blank());

        let mut mixed = vec![0u8; 400 * 4];
        // One pixel of another colour, far enough in to be sampled.
        mixed[37 * 4] = 255;
        assert!(!frame(mixed, 20, 20).is_blank());
    }

    #[test]
    fn the_fingerprint_notices_a_change_and_ignores_a_repeat() {
        let a = frame(vec![7u8; 4096], 32, 32);
        let b = frame(vec![7u8; 4096], 32, 32);
        assert_eq!(a.fingerprint(), b.fingerprint(), "the same pixels are the same frame");

        let mut changed = vec![7u8; 4096];
        changed[64] = 9;
        assert_ne!(
            a.fingerprint(),
            frame(changed, 32, 32).fingerprint(),
            "a changed pixel must change the hash, or a cache would serve a stale read"
        );

        // Size is part of it: a resized window is a different frame even if
        // the bytes that remain are the same.
        let smaller = frame(vec![7u8; 4096], 64, 16);
        assert_ne!(a.fingerprint(), smaller.fingerprint());
    }
}
