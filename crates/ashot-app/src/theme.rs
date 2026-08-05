//! Claude-desktop-inspired palette: warm dark neutrals + terracotta accent.

use gpui::{rgb, rgba, Rgba};

/// Window background (warm near-black).
pub fn bg() -> Rgba {
    rgb(0x262624)
}
/// Raised surfaces: toolbar, action bar, chips.
pub fn surface() -> Rgba {
    rgb(0x30302e)
}
/// Hovered surface.
pub fn surface_hover() -> Rgba {
    rgb(0x3a3a37)
}
/// Hairline borders.
pub fn border() -> Rgba {
    rgb(0x3d3d3a)
}
/// Primary text (warm off-white).
pub fn text() -> Rgba {
    rgb(0xf5f4ee)
}
/// Secondary text.
pub fn text_muted() -> Rgba {
    rgb(0xa6a39a)
}
/// The terracotta accent.
pub fn accent() -> Rgba {
    rgb(0xd97757)
}
pub fn accent_hover() -> Rgba {
    rgb(0xe08b6d)
}
/// Selection dim over the frozen screenshot.
pub fn scrim() -> Rgba {
    rgba(0x1a1917a8)
}
/// Translucent accent fill inside the selection border.
pub fn accent_fill() -> Rgba {
    rgba(0xd9775722)
}
