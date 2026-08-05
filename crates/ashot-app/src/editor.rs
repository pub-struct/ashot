//! M3: the annotation editor.
//!
//! The same five primitives agents get, drawn by hand. All burning goes
//! through ashot-core's renderer, so human and agent output are identical.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    canvas, div, img, point, prelude::*, px, AnyElement, App, Application, Bounds, Context,
    CursorStyle, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Path, Pixels, Point, RenderImage, SharedString, Size, Window,
    WindowBounds, WindowOptions,
};
use tiny_skia::Pixmap;

use ashot_core::spec::{Annotation, Style};
use ashot_core::Renderer;

use crate::{img::to_render_image, theme};

const TOOLBAR_H: f32 = 52.0;
const STROKES: [(&str, f32); 3] = [("S", 3.0), ("M", 5.0), ("L", 8.0)];
const COLORS: [(&str, u32); 7] = [
    ("red", 0xff3b30),
    ("orange", 0xff9500),
    ("green", 0x34c759),
    ("blue", 0x007aff),
    ("purple", 0xaf52de),
    ("white", 0xffffff),
    ("black", 0x000000),
];

pub fn run(pixmap: Pixmap, source: Option<PathBuf>) -> anyhow::Result<()> {
    Application::with_platform(gpui_platform::current_platform(false)).run(move |cx: &mut App| {
        open_window(pixmap, source, cx);
        cx.activate(true);
    });
    Ok(())
}

pub fn open_window(pixmap: Pixmap, source: Option<PathBuf>, cx: &mut App) {
    let (iw, ih) = (pixmap.width() as f32, pixmap.height() as f32);
    // Width floor = what the compact toolbar needs in one row.
    let win_w = (iw + 32.0).clamp(720.0, 1600.0);
    let win_h = (ih + TOOLBAR_H + 32.0).clamp(440.0, 1000.0);
    let bounds = Bounds::centered(None, Size { width: px(win_w), height: px(win_h) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(SharedString::from("ashot — edit")),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| EditorView::new(pixmap, source, cx));
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            // The launcher app runs in explicit quit mode; make ✕ final here too.
            window.on_window_should_close(cx, |_, cx| {
                cx.quit();
                true
            });
            view
        },
    );
    if window.is_err() {
        eprintln!("failed to open editor window");
        cx.quit();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Rect,
    Ellipse,
    Arrow,
    Marker,
    Text,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Rect => "▭",
            Tool::Ellipse => "◯",
            Tool::Arrow => "↗",
            Tool::Marker => "①",
            Tool::Text => "T",
        }
    }
    fn all() -> [Tool; 5] {
        [Tool::Rect, Tool::Ellipse, Tool::Arrow, Tool::Marker, Tool::Text]
    }
}

struct EditorView {
    base: Pixmap,
    rendered: Arc<RenderImage>,
    annotations: Vec<Annotation>,
    /// Cursor position (image coords) while a drag is in flight. The draft
    /// shape is previewed with GPU elements only — nothing is rasterized
    /// until mouse-up commits it.
    drag_current: Option<(f32, f32)>,
    typing: Option<(f32, f32, String)>,
    tool: Tool,
    color_ix: usize,
    stroke_ix: usize,
    renderer: Renderer,
    source: Option<PathBuf>,
    status: Option<SharedString>,
    focus_handle: FocusHandle,
    drag_start: Option<(f32, f32)>,
}

impl EditorView {
    fn new(pixmap: Pixmap, source: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let rendered = to_render_image(&pixmap);
        Self {
            base: pixmap,
            rendered,
            annotations: Vec::new(),
            drag_current: None,
            typing: None,
            tool: Tool::Rect,
            color_ix: 0,
            stroke_ix: 1,
            renderer: Renderer::new(),
            source,
            status: None,
            focus_handle: cx.focus_handle(),
            drag_start: None,
        }
    }

    fn style(&self) -> Style {
        Style {
            color: Some(COLORS[self.color_ix].0.to_string()),
            stroke_width: Some(STROKES[self.stroke_ix].1),
            fill_opacity: None,
        }
    }

    /// Burn committed annotations onto a fresh copy of the base and swap the
    /// GPU image. Runs only on commits (mouse-up, marker click, text commit,
    /// undo) — never per mouse-move.
    fn re_render(&mut self, cx: &mut Context<Self>) {
        let mut pixmap = self.base.clone();
        if let Err(e) = self.renderer.render(&mut pixmap, &self.annotations) {
            self.status = Some(format!("render error: {e}").into());
        }
        self.rendered = crate::img::into_render_image(pixmap);
        cx.notify();
    }

    fn next_marker_number(&self) -> u32 {
        self.annotations
            .iter()
            .filter_map(|a| match a {
                Annotation::Marker { number, .. } => *number,
                _ => None,
            })
            .max()
            .map_or(1, |m| m + 1)
    }

    /// Aspect-fit placement of the image inside the canvas area (window space).
    fn fit(&self, viewport: Size<Pixels>) -> (f32, f32, f32, f32) {
        let (cw, ch) = (
            f32::from(viewport.width),
            f32::from(viewport.height) - TOOLBAR_H,
        );
        let (iw, ih) = (self.base.width() as f32, self.base.height() as f32);
        let scale = (cw / iw).min(ch / ih).min(2.0);
        let (dw, dh) = (iw * scale, ih * scale);
        let ox = (cw - dw) / 2.0;
        let oy = TOOLBAR_H + (ch - dh) / 2.0;
        (ox, oy, scale, dh.max(1.0))
    }

    fn to_image_coords(&self, pos: Point<Pixels>, viewport: Size<Pixels>) -> Option<(f32, f32)> {
        let (ox, oy, scale, _) = self.fit(viewport);
        let x = (f32::from(pos.x) - ox) / scale;
        let y = (f32::from(pos.y) - oy) / scale;
        if x < 0.0 || y < 0.0 || x > self.base.width() as f32 || y > self.base.height() as f32 {
            return None;
        }
        Some((x, y))
    }

    fn commit_typing(&mut self, cx: &mut Context<Self>) {
        if let Some((x, y, text)) = self.typing.take() {
            if !text.is_empty() {
                self.annotations.push(Annotation::Text {
                    x,
                    y,
                    text,
                    size: Some(24.0),
                    style: self.style(),
                });
            }
        }
        self.re_render(cx);
    }

    fn mouse_down(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        if self.typing.is_some() {
            self.commit_typing(cx);
            return;
        }
        let Some((x, y)) = self.to_image_coords(pos, viewport) else { return };
        self.status = None;
        match self.tool {
            Tool::Marker => {
                self.annotations.push(Annotation::Marker {
                    x,
                    y,
                    number: Some(self.next_marker_number()),
                    size: None,
                    style: self.style(),
                });
                self.re_render(cx);
            }
            Tool::Text => {
                self.typing = Some((x, y, String::new()));
                cx.notify();
            }
            _ => self.drag_start = Some((x, y)),
        }
    }

    fn mouse_move(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        if self.drag_start.is_none() {
            return;
        }
        let Some((x, y)) = self.to_image_coords(pos, viewport) else { return };
        // Cheap: just remember the point and let GPUI repaint the preview
        // elements. The expensive rasterization waits for mouse-up.
        self.drag_current = Some((x, y));
        cx.notify();
    }

    fn mouse_up(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        let Some((sx, sy)) = self.drag_start.take() else { return };
        let (x, y) = self
            .to_image_coords(pos, viewport)
            .or(self.drag_current)
            .unwrap_or((sx, sy));
        self.drag_current = None;
        if (x - sx).abs() > 3.0 || (y - sy).abs() > 3.0 {
            self.annotations.push(self.shape_from_drag(sx, sy, x, y));
            self.re_render(cx);
        } else {
            cx.notify();
        }
    }

    fn shape_from_drag(&self, sx: f32, sy: f32, x: f32, y: f32) -> Annotation {
        let style = self.style();
        match self.tool {
            Tool::Arrow => Annotation::Arrow { from: [sx, sy], to: [x, y], style, label: None },
            Tool::Ellipse => Annotation::Ellipse {
                x: sx.min(x),
                y: sy.min(y),
                w: (x - sx).abs().max(1.0),
                h: (y - sy).abs().max(1.0),
                style,
                label: None,
            },
            _ => Annotation::Rect {
                x: sx.min(x),
                y: sy.min(y),
                w: (x - sx).abs().max(1.0),
                h: (y - sy).abs().max(1.0),
                style,
                label: None,
            },
        }
    }

    fn key_down(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        if let Some((_, _, buf)) = &mut self.typing {
            match key {
                "enter" => self.commit_typing(cx),
                "escape" => {
                    self.typing = None;
                    cx.notify();
                }
                "backspace" => {
                    buf.pop();
                    cx.notify();
                }
                "space" => {
                    buf.push(' ');
                    cx.notify();
                }
                _ => {
                    if let Some(ch) = &ev.keystroke.key_char {
                        if !ev.keystroke.modifiers.control && !ev.keystroke.modifiers.platform {
                            buf.push_str(ch);
                            cx.notify();
                        }
                    }
                }
            }
            return;
        }
        let ctrl = ev.keystroke.modifiers.control;
        match key {
            "z" if ctrl => self.undo(cx),
            "s" if ctrl => self.save(cx),
            "c" if ctrl => self.copy(cx),
            "escape" if self.drag_start.is_some() => {
                self.drag_start = None;
                self.drag_current = None;
                cx.notify();
            }
            "escape" => cx.quit(),
            "r" => self.set_tool(Tool::Rect, cx),
            "o" => self.set_tool(Tool::Ellipse, cx),
            "a" => self.set_tool(Tool::Arrow, cx),
            "m" => self.set_tool(Tool::Marker, cx),
            "t" => self.set_tool(Tool::Text, cx),
            _ => {}
        }
    }

    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.tool = tool;
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if self.typing.take().is_none() {
            self.annotations.pop();
        }
        self.re_render(cx);
    }

    fn burned(&mut self) -> anyhow::Result<Vec<u8>> {
        let mut pixmap = self.base.clone();
        self.renderer.render(&mut pixmap, &self.annotations)?;
        pixmap
            .encode_png()
            .map_err(|e| anyhow::anyhow!("png encode: {e}"))
    }

    fn save_path(&self) -> anyhow::Result<PathBuf> {
        match &self.source {
            Some(input) => {
                let stem = input.file_stem().unwrap_or_default().to_string_lossy();
                Ok(input.with_file_name(format!("{stem}-annotated.png")))
            }
            None => Ok(ashot_core::paths::default_capture_path()?),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let result = self.burned().and_then(|png| {
            let path = self.save_path()?;
            std::fs::write(&path, png)?;
            Ok(path)
        });
        match result {
            Ok(path) => {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true, "action": "save",
                        "path": path.display().to_string(),
                        "annotations": self.annotations.len(),
                    })
                );
                cx.quit();
            }
            Err(e) => {
                self.status = Some(format!("save failed: {e}").into());
                cx.notify();
            }
        }
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let result = self
            .burned()
            .and_then(|png| Ok(ashot_core::clipboard::copy_png(png)?));
        match result {
            Ok(()) => {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true, "action": "copy",
                        "annotations": self.annotations.len(),
                    })
                );
                cx.quit();
            }
            Err(e) => {
                self.status = Some(format!("copy failed: {e}").into());
                cx.notify();
            }
        }
    }

    // --- toolbar pieces ---

    fn tool_button(&self, tool: Tool, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.tool == tool;
        div()
            .id(SharedString::from(format!("tool-{}", tool.label())))
            .w(px(30.))
            .h(px(30.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .text_color(if active { gpui::rgb(0xffffff) } else { theme::text_muted() })
            .when(active, |d| d.bg(theme::accent()))
            .when(!active, |d| d.hover(|s| s.bg(theme::surface_hover())))
            .child(tool.label())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| this.set_tool(tool, cx)),
            )
    }

    fn swatch(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, hex) = COLORS[ix];
        let active = self.color_ix == ix;
        div()
            .id(("swatch", ix))
            .w(px(18.))
            .h(px(18.))
            .flex_none()
            .rounded_full()
            .cursor(CursorStyle::PointingHand)
            .bg(gpui::rgb(hex))
            .border_2()
            .border_color(if active { theme::accent() } else { theme::border() })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.color_ix = ix;
                    cx.notify();
                }),
            )
    }

    fn stroke_button(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, _) = STROKES[ix];
        let active = self.stroke_ix == ix;
        div()
            .id(("stroke", ix))
            .w(px(24.))
            .h(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .text_xs()
            .text_color(if active { gpui::rgb(0xffffff) } else { theme::text_muted() })
            .when(active, |d| d.bg(theme::accent()))
            .when(!active, |d| d.hover(|s| s.bg(theme::surface_hover())))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.stroke_ix = ix;
                    cx.notify();
                }),
            )
    }

    fn text_button(
        &self,
        id: &'static str,
        label: &'static str,
        accented: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .text_sm()
            .when(accented, |d| {
                d.bg(theme::accent())
                    .text_color(gpui::white())
                    .hover(|s| s.bg(theme::accent_hover()))
            })
            .when(!accented, |d| {
                d.text_color(theme::text())
                    .hover(|s| s.bg(theme::surface_hover()))
            })
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| handler(this, cx)),
            )
    }

    /// Small icon action (undo, copy) — square, quiet until hovered.
    fn icon_button(
        &self,
        id: &'static str,
        glyph: &'static str,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .w(px(30.))
            .h(px(30.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .text_sm()
            .text_color(theme::text_muted())
            .hover(|s| s.bg(theme::surface_hover()).text_color(theme::text()))
            .child(glyph)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| handler(this, cx)),
            )
    }

    fn separator(&self) -> impl IntoElement {
        div().w(px(1.)).h(px(22.)).bg(theme::border()).mx_1()
    }

    // --- GPU-side previews (no rasterization during drag/typing) ---

    /// The in-flight shape, drawn as GPUI elements in window space.
    fn drag_preview(&self, ox: f32, oy: f32, scale: f32) -> Option<AnyElement> {
        let (sx, sy) = self.drag_start?;
        let (cx_, cy_) = self.drag_current?;
        let color = gpui::rgb(COLORS[self.color_ix].1);
        let stroke_w = (STROKES[self.stroke_ix].1 * scale).max(1.0);
        let to_win = |x: f32, y: f32| (ox + x * scale, oy + y * scale);

        match self.tool {
            Tool::Rect | Tool::Ellipse => {
                let (x0, y0) = to_win(sx.min(cx_), sy.min(cy_));
                let w = (cx_ - sx).abs() * scale;
                let h = (cy_ - sy).abs() * scale;
                let d = div()
                    .absolute()
                    .left(px(x0))
                    .top(px(y0))
                    .w(px(w.max(1.0)))
                    .h(px(h.max(1.0)))
                    .border_color(color);
                let d = match stroke_w.round() as u32 {
                    0..=1 => d.border_1(),
                    2..=3 => d.border_2(),
                    4..=5 => d.border_4(),
                    _ => d.border_8(),
                };
                let d = if self.tool == Tool::Ellipse { d.rounded_full() } else { d };
                Some(d.into_any_element())
            }
            Tool::Arrow => {
                let (fx, fy) = to_win(sx, sy);
                let (tx, ty) = to_win(cx_, cy_);
                Some(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| paint_arrow(window, fx, fy, tx, ty, stroke_w, color),
                    )
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_full()
                    .into_any_element(),
                )
            }
            // Click tools have no drag preview.
            Tool::Marker | Tool::Text => None,
        }
    }

    /// Live text-under-the-cursor while typing; burned only on Enter.
    fn typing_preview(&self, ox: f32, oy: f32, scale: f32) -> Option<AnyElement> {
        let (x, y, buf) = self.typing.as_ref()?;
        Some(
            div()
                .absolute()
                .left(px(ox + x * scale))
                .top(px(oy + y * scale))
                .text_color(gpui::rgb(COLORS[self.color_ix].1))
                .text_size(px(24.0 * scale))
                .child(format!("{buf}▎"))
                .into_any_element(),
        )
    }
}

/// Fill-based arrow (shaft quad + head triangle) via the low-level paint API.
fn paint_arrow(
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
    // max-then-min, not clamp: clamp panics when len*0.5 < 10 (short arrows).
    let head_len = (stroke_w * 4.0).max(10.0).min(len * 0.5);
    let head_w = head_len * 0.7;
    let (bx, by) = (tx - ux * head_len, ty - uy * head_len);
    let hs = (stroke_w / 2.0).max(0.5);

    let mut shaft = Path::new(point(px(fx + nx * hs), px(fy + ny * hs)));
    shaft.line_to(point(px(bx + nx * hs), px(by + ny * hs)));
    shaft.line_to(point(px(bx - nx * hs), px(by - ny * hs)));
    shaft.line_to(point(px(fx - nx * hs), px(fy - ny * hs)));
    window.paint_path(shaft, color);

    let mut head = Path::new(point(px(tx), px(ty)));
    head.line_to(point(px(bx + nx * head_w / 2.0), px(by + ny * head_w / 2.0)));
    head.line_to(point(px(bx - nx * head_w / 2.0), px(by - ny * head_w / 2.0)));
    window.paint_path(head, color);
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let (ox, oy, scale, _) = self.fit(viewport);
        let (dw, dh) = (
            self.base.width() as f32 * scale,
            self.base.height() as f32 * scale,
        );

        let toolbar = div()
            .absolute()
            .left_0()
            .top_0()
            .w(viewport.width)
            .h(px(TOOLBAR_H))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .overflow_hidden()
            .bg(theme::surface())
            .border_b_1()
            .border_color(theme::border())
            .children(Tool::all().map(|t| self.tool_button(t, cx)))
            .child(self.separator())
            .children((0..COLORS.len()).map(|i| self.swatch(i, cx)))
            .child(self.separator())
            .children((0..STROKES.len()).map(|i| self.stroke_button(i, cx)))
            .child(div().flex_1())
            .child(self.icon_button("undo", "↩", cx, |this, cx| this.undo(cx)))
            .child(self.icon_button("copy", "⧉", cx, |this, cx| this.copy(cx)))
            .child(self.text_button("save", "Save", true, cx, |this, cx| this.save(cx)));

        // Transient messages float over the canvas instead of squeezing the toolbar.
        let status_chip = self.status.clone().map(|status| {
            div()
                .absolute()
                .bottom(px(12.))
                .left_0()
                .w(viewport.width)
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme::surface())
                        .border_1()
                        .border_color(theme::border())
                        .shadow_lg()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(status),
                )
        });

        div()
            .id("editor")
            .relative()
            .size_full()
            .bg(theme::bg())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key_down(ev, cx)))
            .child(
                img(self.rendered.clone())
                    .absolute()
                    .left(px(ox))
                    .top(px(oy))
                    .w(px(dw))
                    .h(px(dh)),
            )
            .children(self.drag_preview(ox, oy, scale))
            .children(self.typing_preview(ox, oy, scale))
            .children(status_chip)
            .child(
                // Input layer over the canvas region only.
                div()
                    .id("canvas")
                    .absolute()
                    .left(px(ox))
                    .top(px(oy))
                    .w(px(dw))
                    .h(px(dh))
                    .cursor(CursorStyle::Crosshair)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                            this.mouse_down(ev.position, window.viewport_size(), cx)
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                        this.mouse_move(ev.position, window.viewport_size(), cx)
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                            this.mouse_up(ev.position, window.viewport_size(), cx)
                        }),
                    ),
            )
            .child(toolbar)
    }
}
