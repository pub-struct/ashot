//! Preview-pane geometry and interaction: letterbox fit, video<->window
//! coordinate mapping, frame compositing (annotations burned in for
//! display), mouse handlers for drawing annotations, and the draft-shape
//! canvas painter. Moved verbatim (module-qualified) from the pre-refactor
//! `video_editor.rs`.

use gpui::{point, px, Context, Path as GpuiPath, Pixels, Point, Size, Window};

use ashot_core::spec::Annotation;

use super::inspector::INSPECTOR_W;
use super::state::{self, Selection};
use super::timeline::TIMELINE_H;
use super::{Tool, VideoEditor, TOOLBAR_H};

/// Window-pixel tolerance for grabbing a crop-rect corner (level-resize)
/// instead of dragging the rect body (recenter).
pub(super) const CORNER_HIT_PX: f32 = 10.0;

/// In-flight preview-canvas crop-rect drag for the selected zoom segment.
/// Captures the pre-drag snapshot so, like timeline drags, it commits to
/// history exactly once, on `mouse_up`.
#[derive(Clone)]
pub(super) enum PreviewDrag {
    MoveCenter { ix: usize, grab_dx: f64, grab_dy: f64, before: state::EditState },
    ResizeLevel { ix: usize, before: state::EditState },
}

impl VideoEditor {
    // ---- geometry ----

    pub(super) fn fit(&self, viewport: Size<Pixels>) -> (f32, f32, f32) {
        let (cw, ch) = (
            f32::from(viewport.width) - INSPECTOR_W,
            f32::from(viewport.height) - TOOLBAR_H - TIMELINE_H,
        );
        let (iw, ih) = (self.info.width as f32, self.info.height as f32);
        let scale = (cw / iw).min(ch / ih);
        let ox = (cw - iw * scale) / 2.0;
        // No `TOOLBAR_H` offset here: the preview column is now a flex child
        // placed below the toolbar by the root flex layout, so `oy` is
        // relative to that column's own top, not the whole window.
        let oy = (ch - ih * scale) / 2.0;
        (ox, oy, scale)
    }

    /// Mouse events on the preview arrive as window-absolute coordinates
    /// (`ev.position`, per GPUI's docs on `MouseDownEvent`/`MouseMoveEvent`/
    /// `MouseUpEvent`), never translated per-element. `fit()`'s `oy` is
    /// deliberately relative to the preview column's own top (it's used to
    /// place children *inside* that column, which the root flex layout has
    /// already shifted down by `TOOLBAR_H`) — so converting an event position
    /// with `fit()`'s offsets directly is off by `TOOLBAR_H`. Every hit-test
    /// or coordinate conversion driven by a raw event position must go
    /// through this first.
    fn window_pos_to_preview_local(pos: Point<Pixels>) -> Point<Pixels> {
        point(pos.x, pos.y - px(TOOLBAR_H))
    }

    pub(super) fn to_video_coords(
        &self,
        pos: Point<Pixels>,
        viewport: Size<Pixels>,
    ) -> Option<(f32, f32)> {
        let pos = Self::window_pos_to_preview_local(pos);
        let (ox, oy, scale) = self.fit(viewport);
        let x = (f32::from(pos.x) - ox) / scale;
        let y = (f32::from(pos.y) - oy) / scale;
        if x < 0.0 || y < 0.0 || x > self.info.width as f32 || y > self.info.height as f32 {
            return None;
        }
        Some((x, y))
    }

    /// Burn current annotations (+typing preview) onto the frame for display.
    pub(super) fn refresh_display(&mut self, cx: &mut Context<Self>) {
        let Some(base) = &self.frame_native else { return };
        let mut shown = base.clone();
        let mut all = self.state.annotations.clone();
        if let Some((x, y, text)) = &self.typing {
            all.push(Annotation::Text {
                x: *x,
                y: *y,
                text: format!("{text}▎"),
                size: Some(28.0),
                style: self.style(),
            });
        }
        if !all.is_empty() {
            let _ = self.renderer.render(&mut shown, &all);
        }
        // Live zoom is NOT applied here: `render()` scales/offsets this full
        // frame at layout time from `state::crop_rect_for` (the shared port
        // of the export math), so zoom drags never pay a per-event pixmap
        // clone + crop + texture re-upload.
        self.frame = Some(crate::img::into_render_image(shown));
        cx.notify();
    }

    // ---- input on preview ----

    pub(super) fn preview_down(
        &mut self,
        pos: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.typing.is_some() {
            self.commit_typing(cx);
            return;
        }
        // Crop-rect-on-preview interaction takes priority when a zoom segment
        // is selected — dragging the rect body recenters it, dragging a
        // corner changes its level. See `crop_rect_down`.
        if let Selection::Zoom(ix) = self.selection {
            if self.crop_rect_down(ix, pos, viewport, cx) {
                return;
            }
        }
        let Some((x, y)) = self.to_video_coords(pos, viewport) else { return };
        // A plain click on the preview with no tool armed deselects whatever
        // timeline item is selected (crop-rect hit-testing above already
        // returned early if the click landed on the selected zoom's rect).
        if self.tool.is_none() && self.selection != Selection::None {
            self.selection = Selection::None;
            cx.notify();
            return;
        }
        match self.tool {
            Some(Tool::Marker) => {
                let number = self
                    .state
                    .annotations
                    .iter()
                    .filter_map(|a| match a {
                        Annotation::Marker { number, .. } => *number,
                        _ => None,
                    })
                    .max()
                    .map_or(1, |m| m + 1);
                self.state.annotations.push(Annotation::Marker {
                    x,
                    y,
                    number: Some(number),
                    size: None,
                    style: self.style(),
                });
                self.history.commit(self.state.clone());
                self.refresh_display(cx);
            }
            Some(Tool::Text) => {
                self.typing = Some((x, y, String::new()));
                self.refresh_display(cx);
            }
            Some(_) => self.drag_start = Some((x, y)),
            None => {}
        }
    }

    pub(super) fn preview_move(
        &mut self,
        pos: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.preview_drag.is_some() {
            self.crop_rect_move(pos, viewport, cx);
            return;
        }
        if self.drag_start.is_none() {
            return;
        }
        if let Some(p) = self.to_video_coords(pos, viewport) {
            self.drag_current = Some(p);
            cx.notify();
        }
    }

    pub(super) fn preview_up(
        &mut self,
        pos: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.preview_drag.is_some() {
            self.crop_rect_up(cx);
            return;
        }
        let Some((sx, sy)) = self.drag_start.take() else { return };
        let (x, y) = self.to_video_coords(pos, viewport).or(self.drag_current).unwrap_or((sx, sy));
        self.drag_current = None;
        if (x - sx).abs() > 3.0 || (y - sy).abs() > 3.0 {
            let style = self.style();
            let annotation = match self.tool {
                Some(Tool::Arrow) => {
                    Annotation::Arrow { from: [sx, sy], to: [x, y], style, label: None }
                }
                Some(Tool::Ellipse) => Annotation::Ellipse {
                    x: sx.min(x),
                    y: sy.min(y),
                    w: (x - sx).abs(),
                    h: (y - sy).abs(),
                    style,
                    label: None,
                },
                _ => Annotation::Rect {
                    x: sx.min(x),
                    y: sy.min(y),
                    w: (x - sx).abs(),
                    h: (y - sy).abs(),
                    style,
                    label: None,
                },
            };
            self.state.annotations.push(annotation);
            self.history.commit(self.state.clone());
        }
        self.refresh_display(cx);
    }

    pub(super) fn commit_typing(&mut self, cx: &mut Context<Self>) {
        if let Some((x, y, text)) = self.typing.take() {
            if !text.is_empty() {
                self.state.annotations.push(Annotation::Text {
                    x,
                    y,
                    text,
                    size: Some(28.0),
                    style: self.style(),
                });
                self.history.commit(self.state.clone());
            }
        }
        self.refresh_display(cx);
    }

    // ---- crop-rect-on-preview (selected zoom segment's peak framing) ----

    /// This segment's *peak* crop rect (`cx, cy, level` at its own center
    /// time), evaluated directly rather than through the eased `zoom_at`
    /// search — the rect represents the edit target the user is framing, not
    /// the currently-playing moment.
    pub(super) fn peak_crop_rect(&self, ix: usize) -> Option<(f64, f64, f64, f64)> {
        let z = self.state.zooms.get(ix)?;
        let (w, h) = (self.info.width as f64, self.info.height as f64);
        let (vw, vh) = (w / z.level, h / z.level);
        let x0 = (z.cx - vw / 2.0).max(0.0).min((w - vw).max(0.0));
        let y0 = (z.cy - vh / 2.0).max(0.0).min((h - vh).max(0.0));
        Some((x0, y0, vw, vh))
    }

    /// Returns `true` if `pos` hit the crop rect (body or a corner) and a
    /// `PreviewDrag` was armed.
    fn crop_rect_down(
        &mut self,
        ix: usize,
        pos: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((x0, y0, vw, vh)) = self.peak_crop_rect(ix) else { return false };
        let (ox, oy, scale) = self.fit(viewport);
        let to_win = |x: f64, y: f64| (ox + x as f32 * scale, oy + y as f32 * scale);
        let (wx0, wy0) = to_win(x0, y0);
        let (wx1, wy1) = to_win(x0 + vw, y0 + vh);
        let local = Self::window_pos_to_preview_local(pos);
        let (px_, py_) = (f32::from(local.x), f32::from(local.y));
        if px_ < wx0 - CORNER_HIT_PX || px_ > wx1 + CORNER_HIT_PX || py_ < wy0 - CORNER_HIT_PX
            || py_ > wy1 + CORNER_HIT_PX
        {
            return false;
        }
        let near_corner = [(wx0, wy0), (wx1, wy0), (wx0, wy1), (wx1, wy1)]
            .iter()
            .any(|(cx_, cy_)| ((px_ - cx_).powi(2) + (py_ - cy_).powi(2)).sqrt() <= CORNER_HIT_PX);
        let before = self.state.clone();
        if near_corner {
            self.preview_drag = Some(PreviewDrag::ResizeLevel { ix, before });
        } else {
            let Some((vx, vy)) = self.to_video_coords(pos, viewport) else { return false };
            let z = &self.state.zooms[ix];
            self.preview_drag = Some(PreviewDrag::MoveCenter {
                ix,
                grab_dx: vx as f64 - z.cx,
                grab_dy: vy as f64 - z.cy,
                before,
            });
        }
        cx.notify();
        true
    }

    fn crop_rect_move(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        let (w, h) = (self.info.width as f64, self.info.height as f64);
        match self.preview_drag.clone() {
            Some(PreviewDrag::MoveCenter { ix, grab_dx, grab_dy, .. }) => {
                if let Some((vx, vy)) = self.to_video_coords(pos, viewport) {
                    self.state.move_zoom_center(ix, vx as f64 - grab_dx, vy as f64 - grab_dy, w, h);
                    // Zoom is applied at render time; a relayout is enough.
                    cx.notify();
                }
            }
            Some(PreviewDrag::ResizeLevel { ix, .. }) => {
                let Some(z) = self.state.zooms.get(ix) else { return };
                let (cx0, cy0) = (z.cx, z.cy);
                if let Some((vx, vy)) = self.to_video_coords(pos, viewport) {
                    let (dx, dy) = ((vx as f64 - cx0).abs(), (vy as f64 - cy0).abs());
                    // level = min(W/(2|dx|), H/(2|dy|)) — corner distance from
                    // center drives the aspect-locked zoom level directly.
                    let lvl_x = if dx > 1.0 { w / (2.0 * dx) } else { 8.0 };
                    let lvl_y = if dy > 1.0 { h / (2.0 * dy) } else { 8.0 };
                    let level = lvl_x.min(lvl_y).clamp(1.0, 8.0);
                    self.state.set_zoom_level(ix, level);
                    cx.notify();
                }
            }
            None => {}
        }
    }

    fn crop_rect_up(&mut self, cx: &mut Context<Self>) {
        if let Some(drag) = self.preview_drag.take() {
            match drag {
                PreviewDrag::MoveCenter { before, .. } | PreviewDrag::ResizeLevel { before, .. } => {
                    let _ = before;
                }
            }
            self.history.commit(self.state.clone());
            cx.notify();
        }
    }
}

/// Fill-based arrow preview (same construction as the image editor's).
pub(super) fn paint_arrow(
    window: &mut Window,
    fx: f32,
    fy: f32,
    tx: f32,
    ty: f32,
    stroke_w: f32,
    color: gpui::Rgba,
) {
    let (dx, dy) = (tx - fx, ty - fy);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 2.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (nx, ny) = (-uy, ux);
    let head_len = (stroke_w * 4.0).max(10.0).min(len * 0.5);
    let head_w = head_len * 0.7;
    let (bx, by) = (tx - ux * head_len, ty - uy * head_len);
    let hs = (stroke_w / 2.0).max(0.5);

    let mut shaft = GpuiPath::new(point(px(fx + nx * hs), px(fy + ny * hs)));
    shaft.line_to(point(px(bx + nx * hs), px(by + ny * hs)));
    shaft.line_to(point(px(bx - nx * hs), px(by - ny * hs)));
    shaft.line_to(point(px(fx - nx * hs), px(fy - ny * hs)));
    window.paint_path(shaft, color);

    let mut head = GpuiPath::new(point(px(tx), px(ty)));
    head.line_to(point(px(bx + nx * head_w / 2.0), px(by + ny * head_w / 2.0)));
    head.line_to(point(px(bx - nx * head_w / 2.0), px(by - ny * head_w / 2.0)));
    window.paint_path(head, color);
}
