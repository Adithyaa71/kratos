//! Finding the things that are not in any tree.
//!
//! A toolbar of unlabelled icons, a canvas app, a remote desktop: UI
//! Automation reports a window and nothing in it, and OCR finds the words but
//! not the buttons. This finds the buttons — OmniParser v2's icon detector,
//! exported to ONNX, run on whatever onnxruntime the machine already has.
//!
//! Two things are deliberately not bundled, and both for the same reason:
//!
//! * **The weights.** AGPL-3.0, 77MB, and not this repository's to ship. The
//!   detector is off until somebody puts them where this can find them.
//! * **The runtime.** `ort`'s default feature set downloads an onnxruntime
//!   build during `cargo build`, which puts a network fetch in the build and
//!   a binary of somebody else's choosing in the tree. This loads a library
//!   the machine already has instead.
//!
//! A machine with neither has no pixel path, which is exactly what it had
//! before this module existed.

use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::value::Tensor;

use crate::capture::Frame;

/// What the network was trained at.
const SIDE: usize = 640;

/// Below this, a detection is noise. OmniParser's own demo uses 0.05, which
/// is generous: these boxes are candidates for a decision model to choose
/// between, not answers, and a missed control is worse than a spurious one.
const SCORE: f32 = 0.05;

/// Two boxes overlapping by more than this are the same icon seen twice.
const NMS_IOU: f32 = 0.45;

/// How many detections to keep. A dense toolbar has dozens; a thousand is a
/// model that has lost its mind, and passing them on would bury the real
/// ones.
const MAX_BOXES: usize = 200;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Icon {
    /// `[x, y, w, h]` in physical screen pixels, offset by the window origin.
    pub bbox: [i32; 4],
    pub score: f32,
}

pub struct Detector {
    session: Session,
    pub model: PathBuf,
    /// The last frame detected, so an unchanged window is not detected
    /// twice. Four hundred milliseconds of CPU is worth not spending on a
    /// screen nobody touched.
    last: Option<(u64, Vec<Icon>)>,
    /// Where the last detection's time went: the resize this code does,
    /// against the network onnxruntime runs. Reported because the two have
    /// completely different fixes.
    pub prepare_ms: u64,
    pub infer_ms: u64,
}

/// Where the weights are, if they are anywhere.
///
/// An explicit path wins; otherwise `models/omniparser/icon_detect.onnx`
/// beside the executable, then under the working directory. Nothing is
/// downloaded: a backend that fetched 77MB of AGPL weights on first run
/// because it felt like it would be a backend nobody could audit.
pub fn weights() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KRATOS_ICON_MODEL") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let rel = Path::new("models").join("omniparser").join("icon_detect.onnx");
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join(&rel);
            if beside.is_file() {
                return Some(beside);
            }
            // A cargo layout puts the executable three directories below the
            // workspace root; this is where the weights live during
            // development.
            for up in [1usize, 2, 3] {
                let mut p = dir.to_path_buf();
                for _ in 0..up {
                    p = p.parent()?.to_path_buf();
                }
                let candidate = p.join(&rel);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    rel.is_file().then_some(rel)
}

/// The onnxruntime library, if the machine has one.
///
/// `ort` reads `ORT_DYLIB_PATH` itself; this only fills it in when the
/// operator has not, so the common case of "the DLL is next to the exe"
/// needs no configuration.
fn locate_runtime() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ORT_DYLIB_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) { "onnxruntime.dll" } else { "libonnxruntime.so" };
    let beside = std::env::current_exe().ok()?.with_file_name(name);
    beside.is_file().then_some(beside)
}

impl Detector {
    /// Load the detector, or say why not.
    pub fn load() -> Result<Detector, String> {
        let model = weights().ok_or_else(|| {
            "no icon detector weights. They are AGPL-3.0 and not shipped here: put \
             icon_detect.onnx in models/omniparser/ beside the executable, or set \
             KRATOS_ICON_MODEL."
                .to_string()
        })?;
        let runtime = locate_runtime().ok_or_else(|| {
            "no onnxruntime library. Put onnxruntime.dll beside the executable, or set \
             ORT_DYLIB_PATH."
                .to_string()
        })?;
        // Safe to set unconditionally: `locate_runtime` returned it either
        // because the operator set it or because the file is really there.
        std::env::set_var("ORT_DYLIB_PATH", &runtime);

        // Threads, explicitly. onnxruntime defaults to one intra-op thread
        // when embedded this way, and a 640x640 convolution stack is the
        // kind of work that divides perfectly: measured 420ms on one thread
        // against the figure in docs/native-benchmarks.md on this machine.
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        let session = Session::builder()
            .map_err(|e| format!("onnxruntime would not start: {e}"))?
            .with_intra_threads(threads)
            .map_err(|e| format!("could not set the thread count: {e}"))?
            .commit_from_file(&model)
            .map_err(|e| format!("could not load {}: {e}", model.display()))?;
        Ok(Detector { session, model, last: None, prepare_ms: 0, infer_ms: 0 })
    }

    /// Find the icons in a captured frame.
    pub fn detect(&mut self, frame: &Frame) -> Result<Vec<Icon>, String> {
        let print = frame.fingerprint();
        if let Some((seen, icons)) = &self.last {
            if *seen == print {
                return Ok(icons.clone());
            }
        }
        let letterbox = Letterbox::fit(frame.width as usize, frame.height as usize);
        let started = std::time::Instant::now();
        let input = letterbox.sample(frame);
        self.prepare_ms = started.elapsed().as_millis() as u64;

        let started = std::time::Instant::now();
        let tensor = Tensor::from_array(([1usize, 3, SIDE, SIDE], input))
            .map_err(|e| format!("could not build the input tensor: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["images" => tensor])
            .map_err(|e| format!("the detector failed: {e}"))?;

        self.infer_ms = started.elapsed().as_millis() as u64;

        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("unreadable output: {e}"))?;

        // [1, 5, N]: centre x, centre y, width, height, score -- one class.
        let n = *shape.last().unwrap_or(&0) as usize;
        if shape.len() != 3 || shape[1] != 5 || n == 0 {
            return Err(format!("unexpected detector output shape {shape:?}"));
        }
        let at = |row: usize, i: usize| data[row * n + i];

        let mut found: Vec<Icon> = Vec::new();
        for i in 0..n {
            let score = at(4, i);
            if score < SCORE {
                continue;
            }
            let (cx, cy, w, h) = (at(0, i), at(1, i), at(2, i), at(3, i));
            if let Some(bbox) = letterbox.back(cx, cy, w, h, frame.origin) {
                found.push(Icon { bbox, score });
            }
        }

        found.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let kept = suppress(found);
        self.last = Some((print, kept.clone()));
        Ok(kept)
    }
}

/// The resize that fits a window into a square without distorting it.
struct Letterbox {
    scale: f32,
    pad_x: f32,
    pad_y: f32,
    src_w: usize,
    src_h: usize,
}

impl Letterbox {
    fn fit(w: usize, h: usize) -> Letterbox {
        let scale = (SIDE as f32 / w.max(1) as f32).min(SIDE as f32 / h.max(1) as f32);
        Letterbox {
            scale,
            pad_x: (SIDE as f32 - w as f32 * scale) / 2.0,
            pad_y: (SIDE as f32 - h as f32 * scale) / 2.0,
            src_w: w,
            src_h: h,
        }
    }

    /// BGRA to the planar RGB the network wants, bilinear.
    ///
    /// Bilinear rather than nearest because the things being found are twelve
    /// pixels across: a nearest-neighbour downsample of a 1920-wide window
    /// drops every other row of a toolbar icon and the detector sees a
    /// different shape than it was trained on.
    fn sample(&self, frame: &Frame) -> Vec<f32> {
        let mut out = vec![0.5f32; 3 * SIDE * SIDE];
        let (w, h) = (self.src_w as f32, self.src_h as f32);
        for y in 0..SIDE {
            let sy = (y as f32 - self.pad_y) / self.scale;
            if sy < 0.0 || sy >= h {
                continue;
            }
            for x in 0..SIDE {
                let sx = (x as f32 - self.pad_x) / self.scale;
                if sx < 0.0 || sx >= w {
                    continue;
                }
                let (r, g, b) = self.bilinear(frame, sx, sy);
                out[y * SIDE + x] = r;
                out[SIDE * SIDE + y * SIDE + x] = g;
                out[2 * SIDE * SIDE + y * SIDE + x] = b;
            }
        }
        out
    }

    fn bilinear(&self, frame: &Frame, sx: f32, sy: f32) -> (f32, f32, f32) {
        let x0 = sx.floor() as usize;
        let y0 = sy.floor() as usize;
        let x1 = (x0 + 1).min(self.src_w - 1);
        let y1 = (y0 + 1).min(self.src_h - 1);
        let fx = sx - x0 as f32;
        let fy = sy - y0 as f32;

        let px = |x: usize, y: usize| -> (f32, f32, f32) {
            let i = (y * self.src_w + x) * 4;
            match frame.bgra.get(i..i + 3) {
                // BGRA on the wire, RGB to the network.
                Some(p) => (p[2] as f32, p[1] as f32, p[0] as f32),
                None => (0.0, 0.0, 0.0),
            }
        };
        let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
        let mix = |p: f32, q: f32, r: f32, s: f32| {
            let top = p + (q - p) * fx;
            let bottom = r + (s - r) * fx;
            (top + (bottom - top) * fy) / 255.0
        };
        (mix(a.0, b.0, c.0, d.0), mix(a.1, b.1, c.1, d.1), mix(a.2, b.2, c.2, d.2))
    }

    /// A box in 640-space back to where it is on the screen.
    fn back(&self, cx: f32, cy: f32, w: f32, h: f32, origin: (i32, i32)) -> Option<[i32; 4]> {
        let x = (cx - w / 2.0 - self.pad_x) / self.scale;
        let y = (cy - h / 2.0 - self.pad_y) / self.scale;
        let bw = w / self.scale;
        let bh = h / self.scale;
        if bw < 2.0 || bh < 2.0 {
            return None;
        }
        // Clamped to the window: a box that hangs off the edge is a box the
        // mouse cannot reach.
        let x0 = x.max(0.0).min(self.src_w as f32);
        let y0 = y.max(0.0).min(self.src_h as f32);
        let x1 = (x + bw).max(0.0).min(self.src_w as f32);
        let y1 = (y + bh).max(0.0).min(self.src_h as f32);
        if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
            return None;
        }
        Some([
            origin.0 + x0 as i32,
            origin.1 + y0 as i32,
            (x1 - x0) as i32,
            (y1 - y0) as i32,
        ])
    }
}

/// How much two rectangles overlap, as a fraction of their union.
pub fn iou(a: [i32; 4], b: [i32; 4]) -> f32 {
    let (ax0, ay0, ax1, ay1) = (a[0], a[1], a[0] + a[2], a[1] + a[3]);
    let (bx0, by0, bx1, by1) = (b[0], b[1], b[0] + b[2], b[1] + b[3]);
    let ix = (ax1.min(bx1) - ax0.max(bx0)).max(0) as f32;
    let iy = (ay1.min(by1) - ay0.max(by0)).max(0) as f32;
    let inter = ix * iy;
    let union = (a[2] * a[3]) as f32 + (b[2] * b[3]) as f32 - inter;
    if union <= 0.0 {
        return 0.0;
    }
    inter / union
}

/// Keep the best box of each cluster. Input must be sorted by score.
fn suppress(sorted: Vec<Icon>) -> Vec<Icon> {
    let mut kept: Vec<Icon> = Vec::new();
    for icon in sorted {
        if kept.len() >= MAX_BOXES {
            break;
        }
        if kept.iter().any(|k| iou(k.bbox, icon.bbox) > NMS_IOU) {
            continue;
        }
        kept.push(icon);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_letterbox_puts_a_wide_window_in_the_middle_of_a_square() {
        let lb = Letterbox::fit(1920, 1080);
        assert!((lb.scale - 640.0 / 1920.0).abs() < 1e-6);
        assert_eq!(lb.pad_x, 0.0, "the wide axis fills the square");
        assert!(lb.pad_y > 0.0, "the short axis is padded");

        // A box covering the whole frame comes back as the whole window.
        let full = lb
            .back(320.0, 320.0, 640.0, 640.0 - 2.0 * lb.pad_y, (0, 0))
            .expect("a box");
        assert_eq!(full[0], 0);
        assert!((full[2] - 1920).abs() <= 2, "{full:?}");
        assert!((full[3] - 1080).abs() <= 2, "{full:?}");
    }

    #[test]
    fn a_box_is_offset_by_where_the_window_is() {
        let lb = Letterbox::fit(640, 640);
        let b = lb.back(320.0, 320.0, 100.0, 50.0, (1000, 500)).expect("a box");
        assert_eq!(b, [1000 + 270, 500 + 295, 100, 50]);
    }

    #[test]
    fn overlap_is_the_fraction_of_the_union() {
        assert_eq!(iou([0, 0, 10, 10], [0, 0, 10, 10]), 1.0);
        assert_eq!(iou([0, 0, 10, 10], [20, 20, 10, 10]), 0.0);
        // Half of each: 50 shared out of 150 covered.
        let half = iou([0, 0, 10, 10], [5, 0, 10, 10]);
        assert!((half - 50.0 / 150.0).abs() < 1e-5, "{half}");
    }

    #[test]
    fn the_same_icon_found_twice_is_kept_once() {
        let icons = vec![
            Icon { bbox: [10, 10, 20, 20], score: 0.9 },
            Icon { bbox: [11, 11, 20, 20], score: 0.8 },
            Icon { bbox: [90, 90, 20, 20], score: 0.7 },
        ];
        let kept = suppress(icons);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(kept[0].score, 0.9, "the best of a cluster survives");
    }

    #[test]
    fn a_missing_model_is_explained_rather_than_a_panic() {
        // The common case on a fresh machine: the weights are AGPL and not
        // shipped, and the message has to say what to do about it.
        std::env::set_var("KRATOS_ICON_MODEL", "Z:\\no-such-model.onnx");
        let found = weights();
        std::env::remove_var("KRATOS_ICON_MODEL");
        assert!(found.is_none(), "a path that is not there is not weights");
    }
}
