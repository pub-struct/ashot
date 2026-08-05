//! Clipboard delivery with GNOME-compatible fallbacks.
//!
//! GNOME's Mutter lacks the data-control protocol wl-clipboard-rs needs, so
//! fall back to the wl-copy binary (hidden-surface trick), then xclip via
//! XWayland (Mutter keeps the two clipboards in sync).

use std::io::Write;

use crate::error::{Error, Result};

pub fn copy_png(png: Vec<u8>) -> Result<()> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    let native = Options::new().copy(
        Source::Bytes(png.clone().into_boxed_slice()),
        MimeType::Specific("image/png".into()),
    );
    if native.is_ok() {
        return Ok(());
    }
    for cmd in [
        &["wl-copy", "--type", "image/png"][..],
        &["xclip", "-selection", "clipboard", "-t", "image/png"][..],
    ] {
        if pipe_to(cmd, &png).is_ok() {
            return Ok(());
        }
    }
    Err(Error::Clipboard(
        "compositor lacks data-control and neither wl-copy nor xclip worked".into(),
    ))
}

fn pipe_to(cmd: &[&str], bytes: &[u8]) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error::Clipboard("no stdin handle".into()))?
        .write_all(bytes)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(Error::Clipboard(format!("{} exited with {status}", cmd[0])));
    }
    Ok(())
}
