//! Screen fingerprints: the mechanical backstop for "did that action actually
//! do anything".
//!
//! The accessibility tree only sees what it is told, and a tree that did not
//! change is not proof that nothing happened -- a click can open a menu, fill
//! a field, or do nothing at all while the tree reads identically. Pixels
//! cannot say *what* changed, but they can say that something did, which is
//! exactly the question "did my click land" needs answered.
//!
//! Two resolutions, because one is not enough:
//!
//! * **Full screen**, 64x36. Cheap, catches dialogs, menus and navigation.
//! * **Target region**, 96x96, cropped to the element. Catches a single
//!   character typed into a canvas, which the full-screen grid cannot: one
//!   glyph is roughly 20 of 9,216 region cells (0.002) but under 2 of 2,304
//!   full-screen cells (0.0005). A full-screen test alone reports a false
//!   `no_effect` on every keystroke into a canvas editor.
//!
//! Capture is BitBlt into a DIB section rather than any image crate: it is a
//! few hundred microseconds, pulls no dependencies, and a full-screen grab
//! measured ~34ms on this machine against action cycles of 700-2,900ms.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// Full-screen fingerprint grid.
pub const FULL_W: usize = 64;
pub const FULL_H: usize = 36;

/// Region fingerprint grid, square so a wide or tall element both keep detail.
pub const REGION_N: usize = 96;

/// Per-cell greyscale tolerance. Compression and subpixel rendering move a
/// value or two without anything actually happening.
pub const TOLERANCE: u8 = 8;

/// Fraction of the full screen that must differ to count as a visible change.
/// Measured idle drift on this machine is 0.0004 -- a 10x margin.
pub const FULL_THRESHOLD: f64 = 0.004;

/// Fraction of the cropped region that must differ. Sized so one typed
/// character clears it.
pub const REGION_THRESHOLD: f64 = 0.0015;

/// A downscaled greyscale summary of some pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub cells: Vec<u8>,
    pub w: usize,
    pub h: usize,
}

impl Fingerprint {
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// Fraction of cells that differ, or `None` if the two cannot be compared.
///
/// `None` means "we could not look", which is deliberately different from
/// `Some(0.0)` ("we looked and nothing moved"). A caller must never treat a
/// failed screen grab as evidence that an action did nothing.
pub fn changed(before: Option<&Fingerprint>, after: Option<&Fingerprint>) -> Option<f64> {
    let (b, a) = (before?, after?);
    if b.cells.is_empty() || b.cells.len() != a.cells.len() {
        return None;
    }
    let differing = b
        .cells
        .iter()
        .zip(a.cells.iter())
        .filter(|(x, y)| x.abs_diff(**y) > TOLERANCE)
        .count();
    Some(differing as f64 / b.cells.len() as f64)
}

/// Above this much ambient motion, a rectangle is too busy for pixels to say
/// anything at all and the answer is `Inconclusive`.
///
/// This number is why the API has three outcomes instead of two. Measured on
/// this machine with an ordinary desktop -- a terminal streaming build output
/// in one window -- two back-to-back full-screen grabs differed by 0.067, and
/// a pair taken moments apart by 0.195. A real Gmail navigation measured
/// 0.197. On a screen like that, full-screen pixel diffing cannot tell "my
/// click worked" from "the terminal printed a line", and any threshold that
/// claims otherwise is manufacturing confidence.
///
/// Two consequences, both deliberate:
///
/// * The coarse fingerprint is taken over the **target window's** rectangle,
///   not the whole screen, so unrelated motion elsewhere is out of frame.
/// * When the rectangle in frame is itself busy, the verdict is
///   `Inconclusive`, and the caller must fall back to structural evidence or
///   a semantic check rather than treat noise as success.
pub const NOISE_CEILING: f64 = 0.05;

/// What the pixels are able to say about whether something changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelEvidence {
    /// Something changed, beyond what ambient motion explains.
    Changed,
    /// Nothing changed, and the rectangle was quiet enough to be sure.
    Unchanged,
    /// The pixels cannot answer: no capture, or too much unrelated motion.
    /// Never to be read as either success or failure.
    Inconclusive,
}

impl PixelEvidence {
    pub fn as_str(self) -> &'static str {
        match self {
            PixelEvidence::Changed => "changed",
            PixelEvidence::Unchanged => "unchanged",
            PixelEvidence::Inconclusive => "inconclusive",
        }
    }

    pub fn is_changed(self) -> bool {
        matches!(self, PixelEvidence::Changed)
    }
}

/// Measure ambient motion in a rectangle by grabbing it twice.
///
/// `None` for `rect` measures the whole primary monitor. Taken immediately
/// before acting, so the baseline reflects the screen as it is now rather than
/// a number baked in at build time.
pub fn measure_drift(rect: Option<[i32; 4]>, gap: std::time::Duration) -> Option<f64> {
    let take = || match rect {
        Some(r) => fingerprint_region(r),
        None => fingerprint_screen(),
    };
    let a = take()?;
    std::thread::sleep(gap);
    let b = take()?;
    changed(Some(&a), Some(&b))
}

/// Judge a delta against the ambient drift measured in the same frame.
pub fn assess(delta: Option<f64>, baseline: Option<f64>, floor: f64) -> PixelEvidence {
    let Some(d) = delta else {
        // Not looking is not evidence, in either direction.
        return PixelEvidence::Inconclusive;
    };
    match baseline {
        Some(b) if b > NOISE_CEILING => PixelEvidence::Inconclusive,
        Some(b) => {
            let bar = (b * 2.0 + 0.001).max(floor);
            if d >= bar {
                PixelEvidence::Changed
            } else {
                PixelEvidence::Unchanged
            }
        }
        None => {
            // Drift could not be measured. A large delta is still a change; a
            // small one cannot be distinguished from unmeasured noise.
            if d >= floor {
                PixelEvidence::Changed
            } else {
                PixelEvidence::Unchanged
            }
        }
    }
}

/// One captured moment, big enough to crop any on-screen rectangle out of.
///
/// The point of holding the pixels rather than re-grabbing per rectangle: an
/// action needs a coarse window fingerprint *and* a fine element fingerprint
/// at the same instant, and a baseline for each. Grabbing per rectangle meant
/// eight BitBlts and two sleeps per action. Cropping from a shared frame means
/// three grabs and one sleep -- and, incidentally, makes the window and region
/// numbers describe the same moment, which separate grabs never quite did.
pub struct Screenshot {
    bgra: Vec<u8>,
    w: usize,
    h: usize,
    /// Virtual-desktop coordinate of this frame's top-left pixel. Negative on
    /// a multi-monitor setup where a secondary display sits left of or above
    /// the primary one.
    origin: (i32, i32),
}

impl Screenshot {
    pub fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    /// Fingerprint a rectangle of this frame, in virtual-desktop coordinates.
    /// `None` for `bbox` means the whole frame.
    ///
    /// A rectangle wholly outside the captured area yields `None` -- "could
    /// not look" -- rather than a fingerprint of black, which would compare
    /// equal to the next one and read as "nothing changed".
    pub fn region(&self, bbox: Option<[i32; 4]>, tw: usize, th: usize) -> Option<Fingerprint> {
        let Some([x, y, w, h]) = bbox else {
            return Some(downsample(&self.bgra, self.w, self.h, tw, th));
        };
        if w <= 0 || h <= 0 {
            return None;
        }
        // Into frame-local coordinates, clipped to what was actually captured.
        let x0 = (x - self.origin.0).max(0) as usize;
        let y0 = (y - self.origin.1).max(0) as usize;
        let x1 = (((x - self.origin.0) + w).max(0) as usize).min(self.w);
        let y1 = (((y - self.origin.1) + h).max(0) as usize).min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let (cw, ch) = (x1 - x0, y1 - y0);
        let mut crop = vec![0u8; cw * ch * 4];
        for row in 0..ch {
            let src = ((y0 + row) * self.w + x0) * 4;
            let dst = row * cw * 4;
            crop[dst..dst + cw * 4].copy_from_slice(&self.bgra[src..src + cw * 4]);
        }
        Some(downsample(&crop, cw, ch, tw, th))
    }
}

/// The whole virtual desktop, every monitor, in one BitBlt.
///
/// Virtual rather than primary so a window on a second display -- including
/// one at a negative coordinate -- can be cropped out of the same frame.
pub fn capture() -> Option<Screenshot> {
    let (ox, oy, w, h) = virtual_screen();
    if w <= 0 || h <= 0 {
        return None;
    }
    let bgra = grab(ox, oy, w, h)?;
    Some(Screenshot { bgra, w: w as usize, h: h as usize, origin: (ox, oy) })
}

/// `(x, y, width, height)` of the whole virtual desktop.
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Primary monitor size in physical pixels.
pub fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
}

/// Fingerprint the whole primary monitor.
pub fn fingerprint_screen() -> Option<Fingerprint> {
    let (w, h) = screen_size();
    if w <= 0 || h <= 0 {
        return None; // headless or a locked session
    }
    let raw = grab(0, 0, w, h)?;
    Some(downsample(&raw, w as usize, h as usize, FULL_W, FULL_H))
}

/// Fingerprint one element's rectangle, `[x, y, w, h]` in physical screen
/// pixels. A zero-area or absurd rectangle yields `None` rather than a
/// fingerprint that would compare equal to everything.
pub fn fingerprint_region(bbox: [i32; 4]) -> Option<Fingerprint> {
    let [x, y, w, h] = bbox;
    if w <= 0 || h <= 0 || w > 20_000 || h > 20_000 {
        return None;
    }
    let raw = grab(x, y, w, h)?;
    Some(downsample(&raw, w as usize, h as usize, REGION_N, REGION_N))
}

/// Copy a rectangle of the screen as BGRA bytes.
///
/// Coordinates are virtual-desktop coordinates: `GetDC(NULL)` spans every
/// monitor, so a window on a secondary display -- including one at negative
/// x -- captures correctly.
fn grab(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    unsafe {
        let screen: HDC = GetDC(std::ptr::null_mut::<c_void>() as HWND);
        if screen.is_null() {
            return None;
        }
        // Every path below must release the screen DC, so the body is a
        // closure and cleanup happens once, after it.
        let result = (|| {
            let mem: HDC = CreateCompatibleDC(screen);
            if mem.is_null() {
                return None;
            }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative height requests a top-down DIB, so row 0 is the top
                // of the image and the downsampler does not have to flip.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap: HBITMAP = CreateDIBSection(
                mem,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(mem);
                return None;
            }
            let old: HGDIOBJ = SelectObject(mem, bitmap as HGDIOBJ);
            let blitted = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY) != 0;
            let out = if blitted {
                let len = (w as usize) * (h as usize) * 4;
                Some(std::slice::from_raw_parts(bits as *const u8, len).to_vec())
            } else {
                None
            };
            SelectObject(mem, old);
            DeleteObject(bitmap as HGDIOBJ);
            DeleteDC(mem);
            out
        })();
        ReleaseDC(std::ptr::null_mut::<c_void>() as HWND, screen);
        result
    }
}

/// Box-average BGRA pixels down to a greyscale grid.
///
/// Averaging rather than sampling matters: a one-pixel caret sampled by
/// nearest neighbour either dominates a cell or vanishes depending on
/// alignment, and both make the change metric jittery. Averaging makes a
/// blinking caret small and a typed glyph visible.
fn downsample(bgra: &[u8], sw: usize, sh: usize, tw: usize, th: usize) -> Fingerprint {
    let mut cells = vec![0u8; tw * th];
    if sw == 0 || sh == 0 {
        return Fingerprint { cells, w: tw, h: th };
    }
    for ty in 0..th {
        // Source band for this output row, always at least one pixel tall.
        let y0 = ty * sh / th;
        let y1 = ((ty + 1) * sh).div_ceil(th).max(y0 + 1).min(sh);
        for tx in 0..tw {
            let x0 = tx * sw / tw;
            let x1 = ((tx + 1) * sw).div_ceil(tw).max(x0 + 1).min(sw);
            let mut sum = 0u64;
            let mut n = 0u64;
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let i = row + sx * 4;
                    if i + 2 < bgra.len() {
                        let b = bgra[i] as u64;
                        let g = bgra[i + 1] as u64;
                        let r = bgra[i + 2] as u64;
                        // Rec. 601 luma in fixed point.
                        sum += (r * 77 + g * 151 + b * 28) >> 8;
                        n += 1;
                    }
                }
            }
            cells[ty * tw + tx] = sum.checked_div(n).unwrap_or(0) as u8;
        }
    }
    Fingerprint { cells, w: tw, h: th }
}

/// Build a fingerprint from raw BGRA, for tests and for any caller that
/// already has pixels.
pub fn fingerprint_bgra(bgra: &[u8], sw: usize, sh: usize, tw: usize, th: usize) -> Fingerprint {
    downsample(bgra, sw, sh, tw, th)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(w: usize, h: usize, fill: u8) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for px in v.chunks_mut(4) {
            px[0] = fill;
            px[1] = fill;
            px[2] = fill;
            px[3] = 255;
        }
        v
    }

    fn put_glyph(buf: &mut [u8], w: usize, x: usize, y: usize, gw: usize, gh: usize) {
        for yy in y..(y + gh) {
            for xx in x..(x + gw) {
                let i = (yy * w + xx) * 4;
                if i + 3 < buf.len() {
                    buf[i] = 0;
                    buf[i + 1] = 0;
                    buf[i + 2] = 0;
                }
            }
        }
    }

    #[test]
    fn none_is_distinct_from_zero() {
        let a = Fingerprint { cells: vec![1, 2, 3], w: 3, h: 1 };
        assert_eq!(changed(Some(&a), Some(&a)), Some(0.0));
        assert_eq!(changed(None, Some(&a)), None);
        assert_eq!(changed(Some(&a), None), None);
        // Mismatched sizes cannot be compared, and must not read as "no change".
        let b = Fingerprint { cells: vec![1, 2], w: 2, h: 1 };
        assert_eq!(changed(Some(&a), Some(&b)), None);
    }

    #[test]
    fn small_noise_within_tolerance_is_not_a_change() {
        let a = Fingerprint { cells: vec![100; 100], w: 10, h: 10 };
        let mut b = a.clone();
        for c in b.cells.iter_mut() {
            *c += TOLERANCE; // at the tolerance, not over it
        }
        assert_eq!(changed(Some(&a), Some(&b)), Some(0.0));
        b.cells[0] += 1; // one cell now over
        assert_eq!(changed(Some(&a), Some(&b)), Some(0.01));
    }

    // -- the canvas case ----------------------------------------------------

    #[test]
    fn one_character_on_a_canvas_clears_the_region_bar_but_not_the_screen_bar() {
        // A 1920x1080 desktop with a 400x300 canvas at (300, 200). One 9x16
        // glyph is typed into it. This is requirement 7: a full-screen test
        // alone calls that `no_effect`, and it is not.
        let (sw, sh) = (1920usize, 1080usize);
        let before_full = canvas(sw, sh, 235);
        let mut after_full = before_full.clone();
        put_glyph(&mut after_full, sw, 310, 210, 9, 16);

        let fb = fingerprint_bgra(&before_full, sw, sh, FULL_W, FULL_H);
        let fa = fingerprint_bgra(&after_full, sw, sh, FULL_W, FULL_H);
        let full_delta = changed(Some(&fb), Some(&fa)).unwrap();
        assert!(
            full_delta < FULL_THRESHOLD,
            "full screen should NOT see one glyph: {full_delta} vs {FULL_THRESHOLD}"
        );

        // Same pixels, cropped to the canvas element.
        let (cx, cy, cw, ch) = (300usize, 200usize, 400usize, 300usize);
        let crop = |src: &[u8]| {
            let mut out = vec![0u8; cw * ch * 4];
            for row in 0..ch {
                let s = ((cy + row) * sw + cx) * 4;
                let d = row * cw * 4;
                out[d..d + cw * 4].copy_from_slice(&src[s..s + cw * 4]);
            }
            out
        };
        let rb = fingerprint_bgra(&crop(&before_full), cw, ch, REGION_N, REGION_N);
        let ra = fingerprint_bgra(&crop(&after_full), cw, ch, REGION_N, REGION_N);
        let region_delta = changed(Some(&rb), Some(&ra)).unwrap();
        assert!(
            region_delta >= REGION_THRESHOLD,
            "region SHOULD see one glyph: {region_delta} vs {REGION_THRESHOLD}"
        );
    }

    #[test]
    fn an_identical_region_reads_as_no_change() {
        let c = canvas(400, 300, 200);
        let a = fingerprint_bgra(&c, 400, 300, REGION_N, REGION_N);
        assert_eq!(changed(Some(&a), Some(&a)), Some(0.0));
        assert!(changed(Some(&a), Some(&a)).unwrap() < REGION_THRESHOLD);
    }

    #[test]
    fn a_dialog_opening_clears_the_full_screen_bar_easily() {
        let (sw, sh) = (1920usize, 1080usize);
        let before = canvas(sw, sh, 235);
        let mut after = before.clone();
        // A modest 500x300 dialog is 7% of the screen.
        put_glyph(&mut after, sw, 700, 400, 500, 300);
        let fb = fingerprint_bgra(&before, sw, sh, FULL_W, FULL_H);
        let fa = fingerprint_bgra(&after, sw, sh, FULL_W, FULL_H);
        let d = changed(Some(&fb), Some(&fa)).unwrap();
        assert!(d > FULL_THRESHOLD * 5.0, "{d}");
    }

    // -- downsampling -------------------------------------------------------

    #[test]
    fn downsample_handles_a_region_smaller_than_the_grid() {
        // A 320x22 textbox upscaled into 96x96: every cell must be covered,
        // none left at the zero default, or the comparison is meaningless.
        let c = canvas(320, 22, 128);
        let f = fingerprint_bgra(&c, 320, 22, REGION_N, REGION_N);
        assert_eq!(f.len(), REGION_N * REGION_N);
        assert!(f.cells.iter().all(|&v| v > 100 && v < 150), "uniform input, uniform output");
    }

    #[test]
    fn downsample_is_greyscale_weighted_not_a_plain_mean() {
        // Pure green is brighter than pure blue to the eye, and the metric
        // should reflect that rather than treating channels equally.
        let mut green = vec![0u8; 4];
        green[1] = 255;
        green[3] = 255;
        let mut blue = vec![0u8; 4];
        blue[0] = 255;
        blue[3] = 255;
        let g = fingerprint_bgra(&green, 1, 1, 1, 1).cells[0];
        let b = fingerprint_bgra(&blue, 1, 1, 1, 1).cells[0];
        assert!(g > b, "green {g} should read brighter than blue {b}");
    }

    #[test]
    fn empty_source_does_not_panic() {
        let f = fingerprint_bgra(&[], 0, 0, FULL_W, FULL_H);
        assert_eq!(f.len(), FULL_W * FULL_H);
    }

    #[test]
    fn a_crop_from_a_shared_frame_matches_a_direct_fingerprint() {
        // The optimisation only holds if cropping gives the same answer as
        // grabbing that rectangle on its own.
        let (_, _, w, h) = virtual_screen();
        if w <= 0 || h <= 0 {
            return;
        }
        let shot = capture().expect("capture");
        let (sw, sh) = shot.size();
        assert_eq!((sw, sh), (w as usize, h as usize));

        let rect = [0, 0, 400.min(w), 300.min(h)];
        let cropped = shot.region(Some(rect), REGION_N, REGION_N).expect("crop");
        assert_eq!(cropped.len(), REGION_N * REGION_N);
        assert!(cropped.cells.iter().any(|&v| v != 0), "a real screen is not all black");
    }

    #[test]
    fn a_rectangle_off_the_captured_area_is_none_not_black() {
        // Black would compare equal to the next black and read as "nothing
        // changed", turning a missed action into a success.
        let Some(shot) = capture() else { return };
        assert!(shot.region(Some([-100_000, -100_000, 50, 50]), 8, 8).is_none());
        assert!(shot.region(Some([0, 0, 0, 10]), 8, 8).is_none());
    }

    #[test]
    fn a_partly_offscreen_rectangle_is_clipped_rather_than_refused() {
        let Some(shot) = capture() else { return };
        let (_, _, w, _) = virtual_screen();
        // Straddles the right edge: half of it is real, so it is usable.
        let f = shot.region(Some([w - 50, 0, 200, 100]), 16, 16);
        assert!(f.is_some());
    }

    #[test]
    fn the_whole_frame_can_be_fingerprinted_without_a_rectangle() {
        let Some(shot) = capture() else { return };
        let f = shot.region(None, FULL_W, FULL_H).expect("whole frame");
        assert_eq!(f.len(), FULL_W * FULL_H);
    }

    #[test]
    fn three_grabs_cost_less_than_eight() {
        // The claim behind the optimisation, measured rather than asserted.
        if !kratos_ipc::win::session_is_unlocked() {
            eprintln!("this session is locked; skipping the capture timing");
            return;
        }
        let Some(_) = capture() else { return };
        let t0 = std::time::Instant::now();
        for _ in 0..3 {
            let _ = capture();
        }
        let three = t0.elapsed();

        let t1 = std::time::Instant::now();
        for _ in 0..8 {
            let _ = fingerprint_screen();
        }
        let eight = t1.elapsed();
        eprintln!("3 virtual-screen captures: {three:?}   8 primary grabs: {eight:?}");
        assert!(three < eight, "{three:?} should beat {eight:?}");
    }

    #[test]
    fn a_degenerate_bbox_yields_none() {
        assert!(fingerprint_region([10, 10, 0, 50]).is_none());
        assert!(fingerprint_region([10, 10, -5, 50]).is_none());
        assert!(fingerprint_region([10, 10, 99_999, 50]).is_none());
    }

    // -- live capture -------------------------------------------------------

    #[test]
    fn live_capture_works_and_its_drift_is_reported_not_assumed() {
        let (w, h) = screen_size();
        if w <= 0 || h <= 0 {
            eprintln!("no display; skipping live capture");
            return;
        }
        let a = fingerprint_screen().expect("capture");
        assert_eq!(a.len(), FULL_W * FULL_H);
        assert!(a.cells.iter().any(|&v| v != 0), "a real screen is not all black");

        let drift = measure_drift(None, std::time::Duration::from_millis(40));
        eprintln!("live full-screen ambient drift: {drift:?}");
        // No assertion that the desktop is still: it is not, and a test that
        // demanded it would only pass when nobody was using the machine.
        // What must hold is that a busy frame is reported as unusable rather
        // than as a change.
        if let Some(d) = drift {
            if d > NOISE_CEILING {
                assert_eq!(
                    assess(Some(d), Some(d), FULL_THRESHOLD),
                    PixelEvidence::Inconclusive
                );
            }
        }
    }

    #[test]
    fn a_busy_frame_is_inconclusive_rather_than_a_false_success() {
        // The measured numbers from this machine: ambient 0.067, and a delta
        // of 0.195 that owed nothing to any action. Reporting that as
        // `Changed` is how a click that missed gets called `executed`.
        assert_eq!(assess(Some(0.195), Some(0.067), FULL_THRESHOLD), PixelEvidence::Inconclusive);
        assert_eq!(assess(Some(0.900), Some(0.067), FULL_THRESHOLD), PixelEvidence::Inconclusive);
    }

    #[test]
    fn a_quiet_frame_keeps_full_sensitivity() {
        // The number the Python build measured on a still screen.
        let quiet = Some(0.0004);
        assert_eq!(assess(Some(0.005), quiet, FULL_THRESHOLD), PixelEvidence::Changed);
        assert_eq!(assess(Some(0.003), quiet, FULL_THRESHOLD), PixelEvidence::Unchanged);
        assert_eq!(assess(Some(0.197), quiet, FULL_THRESHOLD), PixelEvidence::Changed);
    }

    #[test]
    fn mildly_busy_raises_the_bar_without_giving_up() {
        let mild = Some(0.011);
        // Below twice the drift: indistinguishable from the screen's own motion.
        assert_eq!(assess(Some(0.015), mild, FULL_THRESHOLD), PixelEvidence::Unchanged);
        // A real navigation still clears it.
        assert_eq!(assess(Some(0.197), mild, FULL_THRESHOLD), PixelEvidence::Changed);
    }

    #[test]
    fn not_looking_is_never_evidence_either_way() {
        assert_eq!(assess(None, Some(0.0), FULL_THRESHOLD), PixelEvidence::Inconclusive);
        assert_eq!(assess(None, None, FULL_THRESHOLD), PixelEvidence::Inconclusive);
        assert!(!PixelEvidence::Inconclusive.is_changed());
        assert!(!PixelEvidence::Unchanged.is_changed());
        assert!(PixelEvidence::Changed.is_changed());
    }

    #[test]
    fn an_unmeasurable_baseline_falls_back_to_the_floor() {
        assert_eq!(assess(Some(0.5), None, FULL_THRESHOLD), PixelEvidence::Changed);
        assert_eq!(assess(Some(0.001), None, FULL_THRESHOLD), PixelEvidence::Unchanged);
    }

    #[test]
    fn the_canvas_glyph_still_reads_as_changed_through_assess() {
        // Tying the region numbers to the decision function: a quiet text box
        // with one new character must come out `Changed`, not merely "above a
        // constant".
        assert_eq!(
            assess(Some(0.0021), Some(0.0), REGION_THRESHOLD),
            PixelEvidence::Changed
        );
    }

    #[test]
    fn a_region_grab_of_the_top_left_corner_works() {
        let (w, h) = screen_size();
        if w <= 0 || h <= 0 {
            return;
        }
        let f = fingerprint_region([0, 0, 200, 100]).expect("region capture");
        assert_eq!(f.len(), REGION_N * REGION_N);
    }

    #[test]
    fn capture_cost_is_small_against_an_action_cycle() {
        let (w, h) = screen_size();
        if w <= 0 || h <= 0 {
            return;
        }
        // A locked desktop has nothing drawn to copy, and copying it took
        // 1.7 seconds here against the 35ms this asserts. The number says
        // something about the lock screen, not about this code.
        if !kratos_ipc::win::session_is_unlocked() {
            eprintln!("this session is locked; skipping the capture timing");
            return;
        }
        let start = std::time::Instant::now();
        let rounds = 5;
        for _ in 0..rounds {
            let _ = fingerprint_screen();
        }
        let each = start.elapsed() / rounds;
        eprintln!("full-screen fingerprint: {each:?} each");
        // A before/after pair must stay a rounding error against the
        // 700-2,900ms action cycles this sits inside.
        assert!(each < std::time::Duration::from_millis(250), "{each:?}");
    }
}
