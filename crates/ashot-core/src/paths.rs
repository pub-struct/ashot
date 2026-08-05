//! Default output locations shared by CLI and app.

use std::path::PathBuf;

use crate::error::Result;

/// `~/Pictures/Screenshots/shot-YYYYMMDD-HHMMSS.png`, directory created.
pub fn default_capture_path() -> Result<PathBuf> {
    let dir = dirs::picture_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Pictures"))
        .join("Screenshots");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    Ok(dir.join(format!("shot-{stamp}.png")))
}
