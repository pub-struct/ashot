//! ashot-app: the GPUI desktop app (M2 overlay + M3 editor).
//! Everything GPUI-specific stays inside this crate per DESIGN.md.

pub mod editor;
pub mod img;
pub mod overlay;
pub mod theme;

use std::path::PathBuf;

pub enum Mode {
    /// Freeze-frame region selection over a fresh capture.
    Overlay,
    /// Open the annotation editor on an existing PNG.
    Editor(PathBuf),
}

pub fn run(mode: Mode) -> anyhow::Result<()> {
    match mode {
        Mode::Overlay => {
            // Capture BEFORE the app opens so our own window is not in shot.
            let captured = ashot_core::capture::capture_full()?;
            overlay::run(captured.pixmap)
        }
        Mode::Editor(path) => {
            let bytes = std::fs::read(&path)?;
            let pixmap = tiny_skia::Pixmap::decode_png(&bytes)
                .map_err(|e| anyhow::anyhow!("cannot decode {}: {e}", path.display()))?;
            editor::run(pixmap, Some(path))
        }
    }
}
