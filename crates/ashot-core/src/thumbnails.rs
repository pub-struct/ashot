//! Batch thumbnail extraction for the video-lane scrub strip: one GStreamer
//! process seeks to N evenly-spaced timestamps and streams back small
//! (`max_h`-tall) RGBA frames, rather than spawning N processes like
//! `video::extract_frame` does for single-frame fetches.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use tiny_skia::Pixmap;

use crate::error::{Error, Result};

/// One evenly-spaced thumbnail.
#[derive(Debug, Clone)]
pub struct Thumbnail {
    /// Timestamp in seconds this thumbnail was seeked to.
    pub t: f64,
    /// Decoded pixels, downscaled to `max_h` (aspect preserved).
    pub pixmap: Pixmap,
}

const THUMBS_PY: &str = r#"
import sys, gi
gi.require_version('Gst', '1.0')
from gi.repository import Gst

Gst.init(None)
uri, max_h = sys.argv[1], int(sys.argv[2])
timestamps = [int(line) for line in sys.stdin.read().split() if line.strip()]

pipeline = Gst.parse_launch(
    f'uridecodebin uri={uri} ! videoconvert ! videoscale '
    f'! video/x-raw,format=RGBA,height={max_h} ! appsink name=sink sync=false')
sink = pipeline.get_by_name('sink')
pipeline.set_state(Gst.State.PAUSED)
pipeline.get_state(10 * Gst.SECOND)

for t_ns in timestamps:
    pipeline.seek_simple(Gst.Format.TIME,
                         Gst.SeekFlags.FLUSH | Gst.SeekFlags.ACCURATE, t_ns)
    pipeline.get_state(10 * Gst.SECOND)
    sample = sink.emit('pull-preroll')
    if sample is None:
        sys.stdout.buffer.write((0).to_bytes(4, 'little'))
        sys.stdout.buffer.write((0).to_bytes(4, 'little'))
        sys.stdout.buffer.flush()
        continue
    caps = sample.get_caps().get_structure(0)
    w, h = caps.get_value('width'), caps.get_value('height')
    buf = sample.get_buffer()
    ok, info = buf.map(Gst.MapFlags.READ)
    sys.stdout.buffer.write(w.to_bytes(4, 'little'))
    sys.stdout.buffer.write(h.to_bytes(4, 'little'))
    sys.stdout.buffer.write(info.data)
    buf.unmap(info)
    sys.stdout.buffer.flush()

pipeline.set_state(Gst.State.NULL)
"#;

/// Extract `count` frames evenly spaced across `[0, duration_s)`, decoded at
/// `max_h` pixels tall, via a single GStreamer process. Frames that fail to
/// decode (e.g. right at EOF) are skipped rather than failing the batch.
pub fn extract_thumbnails(
    path: &Path,
    duration_s: f64,
    count: usize,
    max_h: u32,
) -> Result<Vec<Thumbnail>> {
    if count == 0 || duration_s <= 0.0 {
        return Ok(Vec::new());
    }
    let script = super::video::helper_path("thumbs.py", THUMBS_PY)?;
    let uri = format!(
        "file://{}",
        path.canonicalize().map_err(|e| Error::Record(e.to_string()))?.display()
    );
    let times_s: Vec<f64> =
        (0..count).map(|i| duration_s * (i as f64 + 0.5) / count as f64).collect();
    let stdin_lines: String = times_s
        .iter()
        .map(|t| ((t.max(0.0) * 1e9) as u64).to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let mut child = Command::new("python3")
        .arg(script)
        .arg(uri)
        .arg(max_h.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Record(format!("python3: {e}")))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_lines.as_bytes())
        .map_err(|e| Error::Record(format!("thumbnail stdin: {e}")))?;

    let mut data = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut data)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(Error::Record("thumbnail extraction failed".into()));
    }

    let mut out = Vec::with_capacity(count);
    let mut off = 0usize;
    for &t in &times_s {
        if off + 8 > data.len() {
            break;
        }
        let w = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        let h = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
        off += 8;
        if w == 0 || h == 0 {
            continue; // sentinel: this seek failed, skip it
        }
        let expected = (w * h * 4) as usize;
        if off + expected > data.len() {
            break;
        }
        if let Some(mut pixmap) = Pixmap::new(w, h) {
            pixmap.data_mut().copy_from_slice(&data[off..off + expected]);
            out.push(Thumbnail { t, pixmap });
        }
        off += expected;
    }
    Ok(out)
}
