//! ashot-app: the GPUI desktop app (M2 overlay + M3 editor).
//! Everything GPUI-specific stays inside this crate per DESIGN.md.

pub mod editor;
pub mod img;
pub mod video_editor;
pub mod launcher;
pub mod overlay;
pub mod recorder;
pub mod theme;

use std::path::PathBuf;

pub enum Mode {
    /// The floating toolbar: screenshot / record × full / crop.
    Launcher,
    /// Open the annotation editor on an existing PNG.
    Editor(PathBuf),
}

pub fn run(mode: Mode) -> anyhow::Result<()> {
    match mode {
        Mode::Launcher => launcher::run(),
        Mode::Editor(path) => {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if matches!(ext.as_str(), "mp4" | "mkv" | "webm" | "mov") {
                return video_editor::run(path);
            }
            let bytes = std::fs::read(&path)?;
            let pixmap = tiny_skia::Pixmap::decode_png(&bytes)
                .map_err(|e| anyhow::anyhow!("cannot decode {}: {e}", path.display()))?;
            editor::run(pixmap, Some(path))
        }
    }
}
