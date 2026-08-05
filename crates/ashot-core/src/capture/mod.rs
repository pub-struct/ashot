//! Capture layer. Portal-based on Linux; the `Captured` type and cropping
//! logic are platform-neutral so a macOS backend can slot in later.

pub mod outputs;
pub mod portal;

use tiny_skia::Pixmap;

use crate::error::{Error, Result};
use crate::render;
pub use outputs::Monitor;

/// A capture plus the metadata agents need to address it.
pub struct Captured {
    pub pixmap: Pixmap,
    /// Ratio of captured pixels to the compositor's logical layout size
    /// (1.0 on non-HiDPI setups, 2.0 on 2x, fractional values possible).
    pub scale_factor: f64,
    /// Image-pixel region this capture was cropped to, if any.
    pub region: Option<(i32, i32, u32, u32)>,
}

/// Capture the full desktop.
pub fn capture_full() -> Result<Captured> {
    let pixmap = portal::capture_full_blocking()?;
    let scale_factor = layout_scale(&pixmap).unwrap_or(1.0);
    Ok(Captured { pixmap, scale_factor, region: None })
}

/// Capture and crop to a region given in image pixels.
pub fn capture_region(x: i32, y: i32, w: u32, h: u32) -> Result<Captured> {
    let full = capture_full()?;
    let pixmap = render::crop(&full.pixmap, x, y, w, h)?;
    Ok(Captured { pixmap, scale_factor: full.scale_factor, region: Some((x, y, w, h)) })
}

/// Capture and crop to one monitor (index as reported by `monitors()`).
pub fn capture_monitor(index: usize) -> Result<Captured> {
    let monitors = outputs::enumerate()?;
    let monitor = monitors
        .get(index)
        .ok_or(Error::MonitorNotFound(index, monitors.len()))?;
    let full = portal::capture_full_blocking()?;

    let (bx, by, bw, _bh) = layout_bbox(&monitors);
    // The composited screenshot spans the logical bounding box; one uniform
    // scale maps logical coords to image pixels. Mixed-DPI multi-monitor
    // layouts are approximated by this single factor.
    let scale = full.width() as f64 / bw as f64;
    let px = ((monitor.logical_x - bx) as f64 * scale).round() as i32;
    let py = ((monitor.logical_y - by) as f64 * scale).round() as i32;
    let pw = (monitor.logical_w as f64 * scale).round() as u32;
    let ph = (monitor.logical_h as f64 * scale).round() as u32;
    let pw = pw.min(full.width().saturating_sub(px.max(0) as u32));
    let ph = ph.min(full.height().saturating_sub(py.max(0) as u32));

    let pixmap = render::crop(&full, px, py, pw, ph)?;
    Ok(Captured {
        pixmap,
        scale_factor: scale,
        region: Some((px, py, pw, ph)),
    })
}

/// List monitors (agents use this to pick `--monitor N`).
pub fn monitors() -> Result<Vec<Monitor>> {
    outputs::enumerate()
}

fn layout_bbox(monitors: &[Monitor]) -> (i32, i32, i32, i32) {
    let min_x = monitors.iter().map(|m| m.logical_x).min().unwrap_or(0);
    let min_y = monitors.iter().map(|m| m.logical_y).min().unwrap_or(0);
    let max_x = monitors.iter().map(|m| m.logical_x + m.logical_w).max().unwrap_or(0);
    let max_y = monitors.iter().map(|m| m.logical_y + m.logical_h).max().unwrap_or(0);
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

fn layout_scale(pixmap: &Pixmap) -> Option<f64> {
    let monitors = outputs::enumerate().ok()?;
    let (_, _, bw, _) = layout_bbox(&monitors);
    if bw <= 0 {
        return None;
    }
    Some(pixmap.width() as f64 / bw as f64)
}
