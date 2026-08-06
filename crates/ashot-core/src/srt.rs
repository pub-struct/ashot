//! SRT parsing for the read-only captions lane — the inverse of
//! `captions::fmt_srt_time`/`captions::generate`'s writer.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Cue {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}

fn parse_ts(s: &str) -> Option<f64> {
    // HH:MM:SS,mmm
    let s = s.trim();
    let (hms, ms) = s.split_once(',')?;
    let mut parts = hms.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    let ms: f64 = ms.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec + ms / 1000.0)
}

/// Parse an SRT file written by `captions::generate`. Returns `Ok(vec![])` if
/// `path` does not exist (no captions generated yet — the normal case, not an
/// error). Malformed individual blocks are skipped.
pub fn parse_srt(path: &Path) -> crate::error::Result<Vec<Cue>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut cues = Vec::new();
    for block in text.split("\r\n\r\n").flat_map(|b| b.split("\n\n")) {
        let mut lines = block.lines();
        let Some(_index) = lines.next() else { continue };
        let Some(times) = lines.next() else { continue };
        let Some((start, end)) = times.split_once("-->") else { continue };
        let (Some(start_s), Some(end_s)) = (parse_ts(start), parse_ts(end)) else { continue };
        let text: String = lines.collect::<Vec<_>>().join("\n");
        if text.trim().is_empty() {
            continue;
        }
        cues.push(Cue { start_s, end_s, text });
    }
    Ok(cues)
}
