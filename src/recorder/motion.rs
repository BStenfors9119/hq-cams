//! Lightweight motion detection for the offline recorder.
//!
//! Full per-frame pixel analysis is too heavy for a Pi Zero, so this keeps the
//! cost down three ways:
//! - **Sampled**: only a few frames per second are examined (`fps`), not all 15.
//! - **DCT-scaled decode**: `jpeg-decoder` decodes the sampled JPEG at ~1/8
//!   resolution (a near-free byproduct of the JPEG DCT), so we never inflate a
//!   full 1296×972 frame just to compare it.
//! - **Coarse grid**: the small decoded image is averaged into a `COLS×ROWS`
//!   luma grid; motion is "enough cells changed by more than a threshold".
//!
//! This is deliberately simple — it flags *that* something moved, not what. It
//! trades precision for cheapness, which is the right call for "don't fill the
//! drive with an empty scene".

use std::io::Cursor;
use std::time::{Duration, Instant};

use jpeg_decoder::{Decoder, PixelFormat};

/// Coarse comparison grid. 40×30 keeps the diff trivial while still localising
/// motion to a few cells (a person crossing the frame lights up ~dozens).
const COLS: usize = 40;
const ROWS: usize = 30;

/// Tunables, sourced from the recorder config.
pub struct MotionOpts {
    /// How many frames per second to analyse.
    pub fps: u32,
    /// Fraction of grid cells that must change to count as motion (0.0–1.0).
    pub sensitivity: f32,
    /// Per-cell luma delta (0–255) above which a cell counts as changed —
    /// filters sensor noise and gentle light drift.
    pub cell_threshold: u8,
}

pub struct MotionDetector {
    interval: Duration,
    last_sample: Option<Instant>,
    sensitivity: f32,
    cell_threshold: u8,
    prev: Option<[u8; COLS * ROWS]>,
}

impl MotionDetector {
    pub fn new(opts: &MotionOpts) -> Self {
        let fps = opts.fps.max(1);
        Self {
            interval: Duration::from_micros(1_000_000 / fps as u64),
            last_sample: None,
            sensitivity: opts.sensitivity,
            cell_threshold: opts.cell_threshold,
            prev: None,
        }
    }

    /// Feed the latest frame. Returns `Some(true/false)` when the frame was
    /// sampled (motion or not), or `None` when it was skipped for rate-limiting
    /// or couldn't be decoded — callers treat `None` as "no new information".
    pub fn update(&mut self, jpeg: &[u8]) -> Option<bool> {
        let now = Instant::now();
        if let Some(last) = self.last_sample {
            if now.duration_since(last) < self.interval {
                return None;
            }
        }
        self.last_sample = Some(now);

        let grid = decode_grid(jpeg)?;
        let motion = match &self.prev {
            Some(prev) => grid_motion(&grid, prev, self.cell_threshold, self.sensitivity),
            None => false, // first sample: nothing to compare against yet
        };
        self.prev = Some(grid);
        Some(motion)
    }
}

/// Motion = a large enough fraction of grid cells changed by more than the
/// per-cell threshold. Split out from `update` so it's unit-testable without a
/// decoder or timing.
fn grid_motion(cur: &[u8], prev: &[u8], cell_threshold: u8, sensitivity: f32) -> bool {
    let changed = cur
        .iter()
        .zip(prev)
        .filter(|(a, b)| a.abs_diff(**b) > cell_threshold)
        .count();
    (changed as f32 / cur.len().max(1) as f32) > sensitivity
}

/// Decode a JPEG at reduced scale and average it into a `COLS×ROWS` luma grid.
fn decode_grid(jpeg: &[u8]) -> Option<[u8; COLS * ROWS]> {
    let mut decoder = Decoder::new(Cursor::new(jpeg));
    // Ask for a few source pixels per grid cell; the decoder rounds up to the
    // nearest DCT scale (1/8, 1/4, …), so this stays cheap.
    decoder.scale((COLS * 4) as u16, (ROWS * 4) as u16).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let channels = match info.pixel_format {
        PixelFormat::L8 => 1,
        PixelFormat::RGB24 => 3,
        PixelFormat::CMYK32 => 4,
        PixelFormat::L16 => return None, // 16-bit luma isn't produced by our MJPEG
    };
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || pixels.len() < w * h * channels {
        return None;
    }

    let mut grid = [0u8; COLS * ROWS];
    for gy in 0..ROWS {
        let y0 = gy * h / ROWS;
        let y1 = (((gy + 1) * h / ROWS).max(y0 + 1)).min(h);
        for gx in 0..COLS {
            let x0 = gx * w / COLS;
            let x1 = (((gx + 1) * w / COLS).max(x0 + 1)).min(w);
            let (mut sum, mut count) = (0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * channels;
                    let lum = if channels >= 3 {
                        // Rec.601 luma from RGB, integer-weighted.
                        (pixels[i] as u32 * 77 + pixels[i + 1] as u32 * 150 + pixels[i + 2] as u32 * 29)
                            >> 8
                    } else {
                        pixels[i] as u32
                    };
                    sum += lum;
                    count += 1;
                }
            }
            grid[gy * COLS + gx] = (sum / count.max(1)) as u8;
        }
    }
    Some(grid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes the repo's sample frame through the real JPEG path and checks the
    /// grid is the right shape and carries actual image content (not all zero).
    #[test]
    fn decode_grid_of_sample_frame() {
        let jpeg = include_bytes!("../../test.jpg");
        let grid = decode_grid(jpeg).expect("sample frame should decode");
        assert_eq!(grid.len(), COLS * ROWS);
        assert!(grid.iter().any(|&c| c != 0), "grid should hold real luma");
    }

    #[test]
    fn identical_grids_are_not_motion() {
        let g = [100u8; COLS * ROWS];
        assert!(!grid_motion(&g, &g, 18, 0.02));
    }

    #[test]
    fn a_single_changed_cell_is_below_sensitivity() {
        let prev = [100u8; COLS * ROWS];
        let mut cur = prev;
        cur[0] = 255; // 1/1200 cells ≈ 0.08% << 2%
        assert!(!grid_motion(&cur, &prev, 18, 0.02));
    }

    #[test]
    fn many_changed_cells_trip_motion() {
        let prev = [100u8; COLS * ROWS];
        let mut cur = prev;
        // Change 5% of cells well above the per-cell threshold.
        for c in cur.iter_mut().take((COLS * ROWS) / 20) {
            *c = 200;
        }
        assert!(grid_motion(&cur, &prev, 18, 0.02));
    }

    #[test]
    fn sub_threshold_changes_are_ignored() {
        let prev = [100u8; COLS * ROWS];
        let cur = [110u8; COLS * ROWS]; // every cell +10, under the 18 threshold
        assert!(!grid_motion(&cur, &prev, 18, 0.02));
    }
}
