//! Screenshot capture via the xdg-desktop-portal Screenshot interface.
//!
//! Non-interactive requests need a one-time user grant on GNOME (stored in the
//! xdg permission store); `ashot setup` triggers that grant.

use ashpd::desktop::screenshot::Screenshot;
use tiny_skia::Pixmap;

use crate::error::{Error, Result};

/// Capture the full desktop (all monitors composited) as a pixmap.
///
/// zbus runs on the async-io reactor (its own threads), so a plain futures
/// block_on drives this fine — and it stays GPUI-compatible (no tokio).
pub fn capture_full_blocking() -> Result<Pixmap> {
    futures::executor::block_on(capture_full())
}

async fn capture_full() -> Result<Pixmap> {
    let response = Screenshot::request()
        .interactive(false)
        .modal(true)
        .send()
        .await
        .map_err(map_ashpd_error)?
        .response()
        .map_err(map_ashpd_error)?;

    let uri = response.uri().clone();
    if uri.scheme() != "file" {
        return Err(Error::PortalUnavailable(format!(
            "portal returned non-file URI: {uri}"
        )));
    }
    let path = uri
        .to_file_path()
        .map_err(|_| Error::PortalUnavailable(format!("unusable portal URI: {uri}")))?;
    let bytes = std::fs::read(&path)?;
    // The portal leaves its copy behind; remove it so captures don't litter.
    let _ = std::fs::remove_file(&path);
    Pixmap::decode_png(&bytes).map_err(|e| Error::Image(e.to_string()))
}

fn map_ashpd_error(err: ashpd::Error) -> Error {
    match &err {
        ashpd::Error::Response(_) => Error::PortalDenied,
        _ => Error::PortalUnavailable(err.to_string()),
    }
}
