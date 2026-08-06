//! Multi-lane timeline: ruler (tick labels) / video track (thumbnails,
//! dimmed removed segments) / zoom lane (segments — populated in Stage 3,
//! rendered empty here) / waveform / read-only captions lane.
//!
//! All lanes share one `TimelineRect` computed fresh each render, so x<->time
//! mapping never drifts back to the old whole-window-width scrub bug.

use gpui::{
    canvas, div, fill, img, prelude::*, px, Bounds, Context, CursorStyle, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Size, Window,
};

use super::state::{Edge, Selection};
use super::VideoEditor;
use crate::theme;

pub const RULER_H: f32 = 20.0;
pub const VIDEO_LANE_H: f32 = 40.0;
pub const ZOOM_LANE_H: f32 = 22.0;
pub const AUDIO_LANE_H: f32 = 28.0;
pub const CAPTION_LANE_H: f32 = 18.0;
pub const LANE_GAP: f32 = 1.0;
pub const TIMELINE_H: f32 =
    RULER_H + VIDEO_LANE_H + ZOOM_LANE_H + AUDIO_LANE_H + CAPTION_LANE_H + LANE_GAP * 4.0;

/// Window-pixel tolerance for grabbing a segment/zoom edge instead of
/// selecting/dragging the body.
pub const EDGE_HIT_PX: f32 = 6.0;

const TICK_LADDER_S: [f64; 10] = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0];

fn pick_tick_interval(duration_s: f64, width_px: f32) -> f64 {
    let max_ticks = (width_px / 60.0).max(2.0) as f64;
    TICK_LADDER_S
        .iter()
        .copied()
        .find(|iv| duration_s / iv <= max_ticks)
        .unwrap_or(*TICK_LADDER_S.last().unwrap())
}

fn fmt_mmss(t: f64) -> String {
    let t = t.max(0.0).round() as i64;
    format!("{}:{:02}", t / 60, t % 60)
}

#[derive(Clone, Copy)]
pub struct TimelineRect {
    pub left: f32,
    pub width: f32,
}

impl TimelineRect {
    pub fn time_to_x(&self, t: f64, duration_s: f64) -> f32 {
        self.left + (t / duration_s.max(0.01)) as f32 * self.width
    }

    /// `window_x` is in window space (same space as `MouseDownEvent::position.x`).
    pub fn x_to_time(&self, window_x: f32, duration_s: f64) -> f64 {
        let frac = ((window_x - self.left) / self.width.max(1.0)).clamp(0.0, 1.0);
        frac as f64 * duration_s
    }
}

/// Which timeline handle is being dragged; captured pre-drag `before` state
/// so drags commit exactly once, on `mouse_up`.
#[derive(Clone)]
pub(super) enum TimelineDrag {
    SegmentEdge { ix: usize, before: super::state::EditState },
    ZoomBody { ix: usize, grab_offset_s: f64, before: super::state::EditState },
    ZoomEdge { ix: usize, edge: Edge, before: super::state::EditState },
}

impl VideoEditor {
    /// Timeline rect in window space. In today's flex layout the timeline
    /// spans the full window width with no left inset — but geometry always
    /// goes through this struct, not raw `viewport.width`, so a future
    /// left-gutter can't silently reintroduce the whole-window scrub bug.
    pub(super) fn timeline_rect(&self, viewport: Size<Pixels>) -> TimelineRect {
        TimelineRect {
            left: 0.0,
            width: f32::from(viewport.width),
        }
    }

    pub(super) fn scrub_to(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        let rect = self.timeline_rect(viewport);
        self.playhead = rect.x_to_time(f32::from(pos.x), self.info.duration_s);
        self.fetch_frame(cx);
        cx.notify();
    }
}

pub(super) fn render_timeline(
    editor: &mut VideoEditor,
    window: &mut Window,
    cx: &mut Context<VideoEditor>,
) -> impl IntoElement {
    let viewport = window.viewport_size();
    let rect = editor.timeline_rect(viewport);
    let duration = editor.info.duration_s.max(0.01);

    let mut root = div()
        .id("timeline")
        .relative()
        .flex_none()
        .w_full()
        .h(px(TIMELINE_H))
        .bg(theme::surface())
        .border_t_1()
        .border_color(theme::border());

    // ---- ruler ----
    let mut ruler = div()
        .id("ruler")
        .absolute()
        .left(px(rect.left))
        .top(px(0.))
        .w(px(rect.width))
        .h(px(RULER_H))
        .bg(theme::bg())
        .cursor(CursorStyle::PointingHand)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                this.scrubbing = true;
                this.scrub_to(ev.position, window.viewport_size(), cx);
            }),
        );
    let interval = pick_tick_interval(duration, rect.width);
    let mut t = 0.0;
    while t <= duration {
        let x = rect.time_to_x(t, duration);
        ruler = ruler.child(
            div()
                .absolute()
                .left(px(x))
                .top(px(0.))
                .w(px(1.))
                .h(px(RULER_H))
                .bg(theme::border()),
        );
        ruler = ruler.child(
            div()
                .absolute()
                .left(px(x + 3.0))
                .top(px(2.))
                .text_size(px(10.))
                .text_color(theme::text_muted())
                .child(fmt_mmss(t)),
        );
        t += interval;
    }
    root = root.child(ruler);

    // ---- video lane ----
    let video_top = RULER_H + LANE_GAP;
    let mut video_lane = div()
        .id("video-lane")
        .absolute()
        .left(px(rect.left))
        .top(px(video_top))
        .w(px(rect.width))
        .h(px(VIDEO_LANE_H))
        .bg(theme::bg())
        .overflow_hidden();

    if let Some(thumbs) = &editor.thumbnails {
        for (t, img_ref) in thumbs {
            let x = rect.time_to_x(*t, duration);
            let slot_w = (rect.width / (thumbs.len().max(1) as f32)).max(1.0);
            video_lane = video_lane.child(
                img(img_ref.clone())
                    .absolute()
                    .left(px(x - slot_w / 2.0))
                    .top(px(0.))
                    .w(px(slot_w))
                    .h(px(VIDEO_LANE_H))
                    .object_fit(gpui::ObjectFit::Cover),
            );
        }
    }

    for (ix, seg) in editor.state.segments.iter().enumerate() {
        let x0 = rect.time_to_x(seg.start, duration);
        let x1 = rect.time_to_x(seg.end, duration);
        let w = (x1 - x0).max(1.0);
        let selected = editor.selection == Selection::VideoSegment(ix);
        let cell = div()
            .id(("vseg", ix))
            .absolute()
            .left(px(x0))
            .top(px(0.))
            .w(px(w))
            .h(px(VIDEO_LANE_H))
            .cursor(CursorStyle::PointingHand)
            .when(seg.removed, |d| d.bg(gpui::rgba(0x1a191799)))
            .when(selected, |d| d.border_2().border_color(theme::accent()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    let rect = this.timeline_rect(window.viewport_size());
                    let t = rect.x_to_time(f32::from(ev.position.x), this.info.duration_s);
                    // Edge-drag if near an internal boundary shared with a neighbor.
                    let edge_x_next = if ix + 1 < this.state.segments.len() {
                        Some(rect.time_to_x(this.state.segments[ix].end, this.info.duration_s))
                    } else {
                        None
                    };
                    if let Some(ex) = edge_x_next {
                        if (f32::from(ev.position.x) - ex).abs() <= EDGE_HIT_PX {
                            this.timeline_drag = Some(TimelineDrag::SegmentEdge {
                                ix,
                                before: this.state.clone(),
                            });
                            cx.notify();
                            return;
                        }
                    }
                    let _ = t;
                    this.selection = Selection::VideoSegment(ix);
                    cx.notify();
                }),
            );
        video_lane = video_lane.child(cell);
    }
    root = root.child(video_lane);

    // ---- zoom lane ----
    let zoom_top = video_top + VIDEO_LANE_H + LANE_GAP;
    let mut zoom_lane = div()
        .id("zoom-lane")
        .absolute()
        .left(px(rect.left))
        .top(px(zoom_top))
        .w(px(rect.width))
        .h(px(ZOOM_LANE_H))
        .bg(theme::surface());

    // "+ Zoom" lane-header affordance: inserts a new zoom segment centered
    // on the playhead (default 3s, frame-center, current preset level), then
    // selects it so the inspector + preview crop rect appear for framing.
    zoom_lane = zoom_lane.child(
        div()
            .id("addzoom")
            .absolute()
            .left(px(rect.left + 2.0))
            .top(px(1.))
            .px_1p5()
            .h(px(ZOOM_LANE_H - 2.0))
            .flex()
            .items_center()
            .rounded_sm()
            .bg(theme::surface_hover())
            .text_size(px(10.))
            .text_color(theme::text_muted())
            .cursor(CursorStyle::PointingHand)
            .child("+ Zoom")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.add_zoom_at_playhead(cx)),
            ),
    );

    for (ix, z) in editor.state.zooms.iter().enumerate() {
        let (start, end) = (z.t - z.duration / 2.0, z.t + z.duration / 2.0);
        let x0 = rect.time_to_x(start, duration);
        let x1 = rect.time_to_x(end, duration);
        let w = (x1 - x0).max(2.0);
        let selected = editor.selection == Selection::Zoom(ix);
        let cell = div()
            .id(("zseg", ix))
            .absolute()
            .left(px(x0))
            .top(px(1.))
            .w(px(w))
            .h(px(ZOOM_LANE_H - 2.0))
            .rounded_sm()
            .bg(theme::accent())
            .opacity(if selected { 1.0 } else { 0.6 })
            .cursor(CursorStyle::PointingHand)
            .child(
                div()
                    .px_1()
                    .text_size(px(9.))
                    .text_color(gpui::rgb(0xffffff))
                    .overflow_hidden()
                    .child(format!("{:.1}×", z.level)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    let rect = this.timeline_rect(window.viewport_size());
                    let px_ = f32::from(ev.position.x);
                    let Some(z) = this.state.zooms.get(ix) else { return };
                    let (start, end) = (z.t - z.duration / 2.0, z.t + z.duration / 2.0);
                    let (ex0, ex1) =
                        (rect.time_to_x(start, this.info.duration_s), rect.time_to_x(end, this.info.duration_s));
                    let before = this.state.clone();
                    if (px_ - ex0).abs() <= EDGE_HIT_PX {
                        this.timeline_drag =
                            Some(TimelineDrag::ZoomEdge { ix, edge: Edge::Start, before });
                    } else if (px_ - ex1).abs() <= EDGE_HIT_PX {
                        this.timeline_drag = Some(TimelineDrag::ZoomEdge { ix, edge: Edge::End, before });
                    } else {
                        let grab_offset_s = rect.x_to_time(px_, this.info.duration_s) - z.t;
                        this.timeline_drag =
                            Some(TimelineDrag::ZoomBody { ix, grab_offset_s, before });
                    }
                    this.selection = Selection::Zoom(ix);
                    cx.notify();
                }),
            );
        zoom_lane = zoom_lane.child(cell);
    }
    root = root.child(zoom_lane);

    // ---- audio lane (waveform) ----
    let audio_top = zoom_top + ZOOM_LANE_H + LANE_GAP;
    let mut audio_lane = div()
        .id("audio-lane")
        .absolute()
        .left(px(rect.left))
        .top(px(audio_top))
        .w(px(rect.width))
        .h(px(AUDIO_LANE_H))
        .bg(theme::bg())
        .cursor(CursorStyle::PointingHand)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                this.scrubbing = true;
                this.scrub_to(ev.position, window.viewport_size(), cx);
            }),
        );
    if let Some(peaks) = editor.peaks.clone() {
        let rect_w = rect.width;
        let rect_left = rect.left;
        audio_lane = audio_lane.child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| paint_waveform(window, bounds, &peaks, rect_left, rect_w),
            )
            .absolute()
            .left(px(rect.left))
            .top(px(0.))
            .w(px(rect.width))
            .h(px(AUDIO_LANE_H)),
        );
    }
    root = root.child(audio_lane);

    // ---- captions lane (read-only) ----
    let caption_top = audio_top + AUDIO_LANE_H + LANE_GAP;
    let mut caption_lane = div()
        .id("caption-lane")
        .absolute()
        .left(px(rect.left))
        .top(px(caption_top))
        .w(px(rect.width))
        .h(px(CAPTION_LANE_H))
        .bg(theme::surface())
        .overflow_hidden();
    for (ix, cue) in editor.cues.iter().enumerate() {
        let x0 = rect.time_to_x(cue.start_s, duration);
        let x1 = rect.time_to_x(cue.end_s, duration);
        caption_lane = caption_lane.child(
            div()
                .id(("cue", ix))
                .absolute()
                .left(px(x0))
                .top(px(0.))
                .w(px((x1 - x0).max(2.0)))
                .h(px(CAPTION_LANE_H))
                .px_1()
                .overflow_hidden()
                .border_l_1()
                .border_color(theme::border())
                .text_size(px(10.))
                .text_color(theme::text_muted())
                .child(cue.text.clone()),
        );
    }
    root = root.child(caption_lane);

    // ---- shared mouse-move/up for drags (segment edges, zoom body/edges) and
    // scrub fallback. Wired at the timeline root (spanning every lane) rather
    // than per-cell so a drag keeps updating even once the pointer leaves the
    // cell/lane it started in — see TimelineDrag doc comment.
    let commit_drag = |this: &mut VideoEditor, cx: &mut Context<VideoEditor>| {
        this.scrubbing = false;
        if let Some(drag) = this.timeline_drag.take() {
            match drag {
                TimelineDrag::SegmentEdge { before, .. } => {
                    let _ = before;
                }
                // `move_zoom`/`resize_zoom_edge` deliberately skip re-sorting
                // the zooms Vec on every intermediate move (see their doc
                // comments) so the drag's `ix` keeps referring to the same
                // zoom for the whole gesture. Re-sort now, once, on commit,
                // and re-resolve the selection against the post-resort order
                // so it keeps pointing at the zoom the user actually dragged.
                TimelineDrag::ZoomBody { ix, before, .. }
                | TimelineDrag::ZoomEdge { ix, before, .. } => {
                    let _ = before;
                    let dragged = this.state.zooms.get(ix).cloned();
                    this.state.resort_zooms();
                    if let Some(z) = dragged {
                        if let Some(new_ix) = this.state.zooms.iter().position(|z2| *z2 == z) {
                            this.selection = Selection::Zoom(new_ix);
                        }
                    }
                }
            }
            this.history.commit(this.state.clone());
        }
        cx.notify();
    };
    root = root
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
            if this.scrubbing {
                this.scrub_to(ev.position, window.viewport_size(), cx);
                return;
            }
            let video_duration = this.info.duration_s;
            let rect = this.timeline_rect(window.viewport_size());
            match this.timeline_drag.clone() {
                Some(TimelineDrag::SegmentEdge { ix, .. }) => {
                    let nb = rect.x_to_time(f32::from(ev.position.x), video_duration);
                    this.state.resize_segment_edge(ix, nb);
                    cx.notify();
                }
                Some(TimelineDrag::ZoomBody { ix, grab_offset_s, .. }) => {
                    let new_t =
                        rect.x_to_time(f32::from(ev.position.x), video_duration) - grab_offset_s;
                    this.state.move_zoom(ix, new_t, video_duration);
                    // Zoom is applied at render time; a relayout is enough.
                    cx.notify();
                }
                Some(TimelineDrag::ZoomEdge { ix, edge, .. }) => {
                    let new_time = rect.x_to_time(f32::from(ev.position.x), video_duration);
                    this.state.resize_zoom_edge(ix, edge, new_time, video_duration);
                    cx.notify();
                }
                None => {}
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseUpEvent, _, cx| commit_drag(this, cx)),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseUpEvent, _, cx| commit_drag(this, cx)),
        );

    // ---- playhead, spanning all lanes ----
    let phx = rect.time_to_x(editor.playhead, duration);
    root = root.child(
        div()
            .absolute()
            .left(px(phx))
            .top(px(0.))
            .w(px(2.))
            .h(px(TIMELINE_H))
            .bg(gpui::rgb(0xffffff)),
    );

    root
}

fn paint_waveform(window: &mut Window, bounds: Bounds<Pixels>, peaks: &[(f32, f32)], left: f32, width: f32) {
    if peaks.is_empty() || width <= 0.0 {
        return;
    }
    let top = f32::from(bounds.origin.y);
    let mid = top + AUDIO_LANE_H / 2.0;
    let color = theme::text_muted();
    let n = peaks.len();
    // Draw at most ~width/2 bars (2px pitch) to keep quad count sane.
    let stride = (n as f32 / (width / 2.0).max(1.0)).ceil().max(1.0) as usize;
    let mut i = 0;
    while i < n {
        let (lo, hi) = peaks[i];
        let x = left + (i as f32 / n as f32) * width;
        let h = ((hi - lo).abs().max(0.02) * (AUDIO_LANE_H / 2.0)).min(AUDIO_LANE_H / 2.0);
        let bar = Bounds {
            origin: gpui::point(px(x), px(mid - h)),
            size: Size { width: px(1.5), height: px(h * 2.0) },
        };
        window.paint_quad(fill(bar, color));
        i += stride;
    }
}
