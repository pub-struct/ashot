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
