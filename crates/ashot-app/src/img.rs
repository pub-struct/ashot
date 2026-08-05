//! tiny-skia Pixmap → gpui RenderImage.
//!
//! GPUI's renderer expects premultiplied BGRA frames; pixmaps are already
//! premultiplied RGBA, so this is a channel swizzle.

use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::smallvec;
use tiny_skia::Pixmap;

pub fn to_render_image(pixmap: &Pixmap) -> Arc<RenderImage> {
    into_render_image(pixmap.clone())
}

/// Crop `src` to the `(x, y, w, h)` source-pixel rect, clamped to the
/// source's bounds. Used for live zoom preview (`state::crop_rect_for`'s
/// output). Manual row-copy — `Pixmap` exposes `data()`/`data_mut()` as flat
/// `&[u8]`/`&mut [u8]`, no cropping primitive of its own.
pub fn crop_pixmap(src: &Pixmap, x: u32, y: u32, w: u32, h: u32) -> Option<Pixmap> {
    let (sw, sh) = (src.width(), src.height());
    let x = x.min(sw.saturating_sub(1));
    let y = y.min(sh.saturating_sub(1));
    let w = w.max(1).min(sw - x);
    let h = h.max(1).min(sh - y);
    let mut out = Pixmap::new(w, h)?;
    let src_data = src.data();
    let out_data = out.data_mut();
    for row in 0..h {
        let src_off = ((y + row) as usize * sw as usize + x as usize) * 4;
        let dst_off = row as usize * w as usize * 4;
        let len = w as usize * 4;
        out_data[dst_off..dst_off + len].copy_from_slice(&src_data[src_off..src_off + len]);
    }
    Some(out)
}

/// Consumes the pixmap: swizzles in place and moves the buffer into the
/// RenderImage — no extra allocation or copy.
pub fn into_render_image(pixmap: Pixmap) -> Arc<RenderImage> {
    let (w, h) = (pixmap.width(), pixmap.height());
    let mut data = pixmap.take();
    for px in data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let buffer = RgbaImage::from_raw(w, h, data).expect("pixmap data length matches dimensions");
    Arc::new(RenderImage::new(smallvec![Frame::new(buffer)]))
}
