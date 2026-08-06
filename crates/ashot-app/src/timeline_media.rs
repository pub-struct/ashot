//! Process-wide, path-keyed cache for the timeline's thumbnails + waveform
//! peaks — computed once per source video so reopening the editor on the
//! same file is instant. Caption cues are NOT cached here: they can change
//! mid-session (once `generate_captions` finishes), so the editor re-derives
//! them directly from the `.srt` path instead (cheap — no gst/process spawn).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use gpui::RenderImage;

/// Everything the timeline lanes need that never changes during a session.
pub struct TimelineMedia {
    pub thumbnails: Vec<(f64, Arc<RenderImage>)>, // (t_s, image), time-ordered
    pub peaks: Arc<Vec<(f32, f32)>>,               // empty = no audio
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Arc<TimelineMedia>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<TimelineMedia>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

/// Cache lookup keyed by canonicalized path. `None` = not computed yet.
pub fn cached(path: &Path) -> Option<Arc<TimelineMedia>> {
    cache().lock().unwrap().get(&key(path)).cloned()
}

/// Store (or overwrite) the computed media for `path`.
pub fn insert(path: &Path, media: Arc<TimelineMedia>) {
    cache().lock().unwrap().insert(key(path), media);
}
