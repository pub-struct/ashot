//! Pure edit state: cuts (as segments), zoom points, annotations, caption
//! burn toggle. No GPUI dependency — this is the snapshot type undo/redo
//! (see `undo.rs`) clones wholesale, and the type export flows through.

use ashot_core::spec::Annotation;
use ashot_core::video::ZoomPoint;

/// A contiguous slice of the video timeline. `segments` always tiles
/// `[0, duration_s]` with no gaps and no overlaps, ascending by `start`.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoSegment {
    pub start: f64,
    pub end: f64,
    /// true = shown dimmed on the video lane, skipped during playback,
    /// excluded from export (folded into `cuts()`).
    pub removed: bool,
}

/// Everything that participates in undo/redo and that export must see.
/// Deliberately small and `Clone`-cheap: snapshot-per-operation, not
/// snapshot-per-frame.
#[derive(Clone, Debug, PartialEq)]
pub struct EditState {
    pub segments: Vec<VideoSegment>,
    pub zooms: Vec<ZoomPoint>,
    pub annotations: Vec<Annotation>,
    pub burn_cc: bool,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Selection {
    #[default]
    None,
    VideoSegment(usize),
    Zoom(usize),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Edge {
    Start,
    End,
}

impl EditState {
    /// Seconds; same order of magnitude as the old 0.05 epsilon at
    /// video_editor.rs:313 (pre-refactor).
    pub const MIN_SEGMENT: f64 = 0.1;

    pub fn new(duration_s: f64) -> Self {
        Self {
            segments: vec![VideoSegment { start: 0.0, end: duration_s, removed: false }],
            zooms: Vec::new(),
            annotations: Vec::new(),
            burn_cc: false,
        }
    }

    /// Split the segment containing `t` into two segments at `t`, both
    /// inheriting the parent's `removed` flag. No-op (returns false) if `t`
    /// is within MIN_SEGMENT of an existing boundary or out of range.
    pub fn split_at(&mut self, t: f64) -> bool {
        let Some(ix) = self.segments.iter().position(|s| t > s.start && t < s.end) else {
            return false;
        };
        let seg = self.segments[ix].clone();
        if t - seg.start < Self::MIN_SEGMENT || seg.end - t < Self::MIN_SEGMENT {
            return false;
        }
        let removed = seg.removed;
        self.segments[ix].end = t;
        self.segments.insert(ix + 1, VideoSegment { start: t, end: seg.end, removed });
        true
    }

    /// Mark segment `ix` removed. Does not delete/merge the Vec entry —
    /// dimmed rendering + playback-skip + export-exclusion all read `removed`.
    pub fn delete_segment(&mut self, ix: usize) {
        if let Some(s) = self.segments.get_mut(ix) {
            s.removed = true;
        }
    }

    /// Un-delete (used by inspector "Restore" action on a selected removed segment).
    pub fn restore_segment(&mut self, ix: usize) {
        if let Some(s) = self.segments.get_mut(ix) {
            s.removed = false;
        }
    }

    /// Drag the shared boundary between segments[ix] and segments[ix+1].
    /// Clamped to [segments[ix].start + MIN_SEGMENT, segments[ix+1].end - MIN_SEGMENT].
    pub fn resize_segment_edge(&mut self, ix: usize, new_boundary: f64) {
        if ix + 1 >= self.segments.len() {
            return;
        }
        let lo = self.segments[ix].start + Self::MIN_SEGMENT;
        let hi = self.segments[ix + 1].end - Self::MIN_SEGMENT;
        let b = new_boundary.clamp(lo, hi);
        self.segments[ix].end = b;
        self.segments[ix + 1].start = b;
    }

    /// Ranges to REMOVE — feeds `keep_ranges` exactly as the old `self.cuts`
    /// field did.
    pub fn cuts(&self) -> Vec<(f64, f64)> {
        self.segments.iter().filter(|s| s.removed).map(|s| (s.start, s.end)).collect()
    }

    /// Identical algorithm to the pre-refactor `keep_ranges` (video_editor.rs:323-339),
    /// substituting `self.cuts()` for the old `self.cuts` field.
    ///
    /// Empty return is reserved for "no cuts were made at all" — EXPORT_PY
    /// (ashot-core/src/video.rs:91) treats an empty `keep` list as shorthand
    /// for "export the whole, unedited video". If cuts *were* made but they
    /// happen to tile the entire duration (the user deleted every segment),
    /// we must NOT return an empty Vec here — that would be indistinguishable
    /// from "no cuts at all" and would silently export the full video instead
    /// of nothing. In that case return a zero-length sentinel range instead:
    /// non-empty (so the Python truthiness fallback doesn't kick in) but
    /// matches no timestamp (`s0 <= t < e0` is never true when `s0 == e0`),
    /// so every frame is dropped.
    pub fn keep_ranges(&self, duration_s: f64) -> Vec<(f64, f64)> {
        let cuts = self.cuts();
        if cuts.is_empty() {
            return Vec::new();
        }
        let mut keep = Vec::new();
        let mut t = 0.0;
        for (s, e) in &cuts {
            if *s > t + 0.05 {
                keep.push((t, *s));
            }
            t = t.max(*e);
        }
        if t + 0.05 < duration_s {
            keep.push((t, duration_s));
        }
        if keep.is_empty() {
            // Everything was cut: not "no edits", but "keep nothing".
            keep.push((0.0, 0.0));
        }
        keep
    }

    // ---- zoom segments ----

    pub fn add_zoom(
        &mut self,
        t: f64,
        cx: f64,
        cy: f64,
        level: f64,
        duration: f64,
        video_duration: f64,
    ) -> usize {
        let mut z = ZoomPoint { t, cx, cy, level, duration };
        self.clamp_zoom_to_neighbors(&mut z, None, video_duration);
        self.zooms.push(z.clone());
        self.resort_zooms();
        self.zooms.iter().position(|z2| *z2 == z).unwrap_or(0)
    }

    pub fn remove_zoom(&mut self, ix: usize) {
        if ix < self.zooms.len() {
            self.zooms.remove(ix);
        }
    }

    /// Deliberately does NOT call `resort_zooms()` — this is called
    /// repeatedly while a timeline drag is in flight (`TimelineDrag::ZoomBody`
    /// keeps referencing `ix` by Vec position for the whole gesture), and
    /// reordering the backing Vec mid-drag would make that stale `ix` start
    /// mutating a *different* zoom the instant the dragged segment's window
    /// crosses a neighbor's. Callers must call `resort_zooms()` explicitly
    /// once the drag commits (see timeline.rs's mouse-up/mouse-up-out commit).
    pub fn move_zoom(&mut self, ix: usize, new_t: f64, video_duration: f64) {
        let Some(mut z) = self.zooms.get(ix).cloned() else { return };
        z.t = new_t;
        self.clamp_zoom_to_neighbors(&mut z, Some(ix), video_duration);
        self.zooms[ix] = z;
    }

    /// Same rationale as `move_zoom`: no `resort_zooms()` here — callers must
    /// resort once the drag commits, not after every intermediate move.
    pub fn resize_zoom_edge(&mut self, ix: usize, edge: Edge, new_time: f64, video_duration: f64) {
        let Some(z) = self.zooms.get(ix).cloned() else { return };
        let (mut start, mut end) = (z.t - z.duration / 2.0, z.t + z.duration / 2.0);
        const MIN_ZOOM_DURATION: f64 = 0.3;
        match edge {
            Edge::Start => start = new_time.min(end - MIN_ZOOM_DURATION),
            Edge::End => end = new_time.max(start + MIN_ZOOM_DURATION),
        }
        let mut z2 = ZoomPoint { t: (start + end) / 2.0, duration: end - start, ..z };
        self.clamp_zoom_to_neighbors(&mut z2, Some(ix), video_duration);
        self.zooms[ix] = z2;
    }

    pub fn set_zoom_level(&mut self, ix: usize, level: f64) {
        if let Some(z) = self.zooms.get_mut(ix) {
            z.level = level.clamp(1.0, 8.0);
        }
    }

    pub fn move_zoom_center(&mut self, ix: usize, cx: f64, cy: f64, video_w: f64, video_h: f64) {
        let Some(z) = self.zooms.get_mut(ix) else { return };
        let (vw, vh) = (video_w / z.level, video_h / z.level);
        z.cx = cx.clamp(vw / 2.0, video_w - vw / 2.0);
        z.cy = cy.clamp(vh / 2.0, video_h - vh / 2.0);
    }

    pub fn resort_zooms(&mut self) {
        self.zooms.sort_by(|a, b| a.t.total_cmp(&b.t));
    }

    /// Clamp `z`'s `[start,end)` window against its immediate neighbors so
    /// zoom segments never overlap (zero-gap touching is the limit).
    /// `self_ix` (if any) is excluded from neighbor search.
    fn clamp_zoom_to_neighbors(&self, z: &mut ZoomPoint, self_ix: Option<usize>, video_duration: f64) {
        let (mut start, mut end) = (z.t - z.duration / 2.0, z.t + z.duration / 2.0);
        start = start.max(0.0);
        end = end.min(video_duration);

        let mut prev_end: f64 = 0.0;
        let mut next_start: f64 = video_duration;
        for (ix, other) in self.zooms.iter().enumerate() {
            if Some(ix) == self_ix {
                continue;
            }
            let (os, oe) = (other.t - other.duration / 2.0, other.t + other.duration / 2.0);
            if oe <= start && oe > prev_end {
                prev_end = oe;
            }
            if os >= end && os < next_start {
                next_start = os;
            }
        }
        start = start.max(prev_end);
        end = end.min(next_start);
        if end < start {
            end = start;
        }
        z.t = (start + end) / 2.0;
        z.duration = end - start;
    }
}

pub const EASE: f64 = 0.6; // matches EXPORT_PY's EASE (ashot-core/src/video.rs:93)

/// Port of EXPORT_PY zoom_at (ashot-core/src/video.rs:110-120).
pub fn zoom_at(t: f64, zooms: &[ZoomPoint], w: f64, h: f64) -> (f64, f64, f64) {
    let mut best = (1.0, w / 2.0, h / 2.0);
    for z in zooms {
        let half = z.duration / 2.0;
        let d = (t - z.t).abs();
        if d > half {
            continue;
        }
        let k = ((half - d) / EASE).min(1.0);
        let k = k * k * (3.0 - 2.0 * k);
        best = (1.0 + (z.level - 1.0) * k, z.cx, z.cy);
    }
    best
}

/// Port of EXPORT_PY crop_for (ashot-core/src/video.rs:122-130), returning
/// source-pixel rect (x0, y0, crop_w, crop_h) instead of GStreamer L/R/T/B
/// margins, and WITHOUT the `even()` pixel-parity rounding (a
/// vapostproc/encoder requirement, not part of the crop math itself).
pub fn crop_rect_for(t: f64, zooms: &[ZoomPoint], w: f64, h: f64) -> Option<(f64, f64, f64, f64)> {
    let (level, cx, cy) = zoom_at(t, zooms, w, h);
    if level <= 1.001 {
        return None;
    }
    let (vw, vh) = (w / level, h / level);
    let x0 = (cx - vw / 2.0).max(0.0).min(w - vw);
    let y0 = (cy - vh / 2.0).max(0.0).min(h - vh);
    Some((x0, y0, vw, vh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_delete_matches_old_cuts_flow() {
        let mut st = EditState::new(10.0);
        assert!(st.split_at(2.0));
        assert!(st.split_at(5.0));
        assert_eq!(st.segments.len(), 3);
        // Delete the middle segment [2,5) — equivalent to the old two-click
        // cut flow producing cuts.push((2.0, 5.0)).
        st.delete_segment(1);
        assert_eq!(st.cuts(), vec![(2.0, 5.0)]);
        assert_eq!(st.keep_ranges(10.0), vec![(0.0, 2.0), (5.0, 10.0)]);
    }

    #[test]
    fn split_at_noop_near_boundary() {
        let mut st = EditState::new(10.0);
        assert!(st.split_at(5.0));
        // Splitting again very close to the new boundary should no-op.
        assert!(!st.split_at(5.05));
        assert_eq!(st.segments.len(), 2);
    }

    #[test]
    fn restore_segment_clears_removed() {
        let mut st = EditState::new(10.0);
        st.split_at(3.0);
        st.delete_segment(0);
        assert_eq!(st.cuts(), vec![(0.0, 3.0)]);
        st.restore_segment(0);
        assert!(st.cuts().is_empty());
    }
}
