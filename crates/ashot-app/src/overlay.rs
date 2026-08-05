//! M2: freeze-frame region selection.
//!
//! Flow (CleanShot-style): drag a region → it is cropped and copied to the
//! clipboard immediately → a preview card floats at the bottom-left with
//! Save / Edit actions, auto-dismissing after a few seconds unless hovered.
//! Wayland note: compositors don't allow positioned windows, so the "floating"
//! card is drawn inside the fullscreen frozen-frame window — visually
//! identical to a real floating panel.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, prelude::*, px, App, Bounds, Context, CursorStyle, FocusHandle,
    Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, RenderImage, Window, WindowBounds, WindowOptions,
};
use tiny_skia::Pixmap;

use crate::{editor, img::to_render_image, theme};

/// How long the preview card lingers before self-dismissing (unless hovered).
const DISMISS_AFTER: Duration = Duration::from_secs(6);

/// What a completed selection does.
#[derive(Clone)]
pub enum Purpose {
    Screenshot,
    Record { height: u32, mic: Option<String> },
}

/// How the overlay starts.
#[derive(Clone)]
pub enum OverlayStart {
    /// Drag-select for the given purpose.
    Select(Purpose),
    /// Jump straight to the preview card with the full frame (already copied).
    PreviewFull,
    /// Jump straight to the preview card with this image-pixel region.
    PreviewRegion((u32, u32, u32, u32)),
}

pub fn open_window(pixmap: Pixmap, start: OverlayStart, cx: &mut App) {
    let display_bounds = cx
        .primary_display()
        .map(|d| d.bounds())
        .unwrap_or_else(|| Bounds::from_corners(Point::default(), Point::new(px(1920.), px(1080.))));
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Fullscreen(display_bounds)),
            titlebar: None,
            is_movable: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| OverlayView::new(pixmap, start, cx));
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            view
        },
    );
    if window.is_err() {
        eprintln!("failed to open overlay window");
        cx.quit();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Dragging,
    Preview,
}

struct PreviewState {
    /// The cropped capture, kept for Save / Edit.
    pixmap: Pixmap,
    thumb: Arc<RenderImage>,
}

struct OverlayView {
    base: Arc<Pixmap>,
    image: Arc<RenderImage>,
    focus_handle: FocusHandle,
    phase: Phase,
    start: Point<Pixels>,
    end: Point<Pixels>,
    preview: Option<PreviewState>,
    preview_hovered: bool,
    dismiss_gen: usize,
    purpose: Purpose,
    /// Some(rect) = enter preview immediately (None rect = full frame).
    start_preview: Option<Option<(i32, i32, u32, u32)>>,
}

impl OverlayView {
    fn new(pixmap: Pixmap, start: OverlayStart, cx: &mut Context<Self>) -> Self {
        let image = to_render_image(&pixmap);
        let (purpose, start_preview) = match start {
            OverlayStart::Select(purpose) => (purpose, None),
            OverlayStart::PreviewFull => (Purpose::Screenshot, Some(None)),
            OverlayStart::PreviewRegion((x, y, w, h)) => {
                (Purpose::Screenshot, Some(Some((x as i32, y as i32, w, h))))
            }
        };
        Self {
            base: Arc::new(pixmap),
            image,
            focus_handle: cx.focus_handle(),
            phase: Phase::Idle,
            start: Point::default(),
            end: Point::default(),
            preview: None,
            preview_hovered: false,
            dismiss_gen: 0,
            purpose,
            start_preview,
        }
    }

    fn selection(&self) -> Option<(Pixels, Pixels, Pixels, Pixels)> {
        if self.phase == Phase::Idle {
            return None;
        }
        let x0 = self.start.x.min(self.end.x);
        let y0 = self.start.y.min(self.end.y);
        let x1 = self.start.x.max(self.end.x);
        let y1 = self.start.y.max(self.end.y);
        if f32::from(x1 - x0) < 3.0 || f32::from(y1 - y0) < 3.0 {
            return None;
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    /// Window-space selection → image/stream pixel rect.
    fn selection_to_image(
        &self,
        window: &Window,
        (x, y, w, h): (Pixels, Pixels, Pixels, Pixels),
    ) -> (i32, i32, u32, u32) {
        let viewport = window.viewport_size();
        let sx = self.base.width() as f32 / f32::from(viewport.width);
        let sy = self.base.height() as f32 / f32::from(viewport.height);
        let ix = (f32::from(x) * sx).round().max(0.0) as i32;
        let iy = (f32::from(y) * sy).round().max(0.0) as i32;
        let iw = ((f32::from(w) * sx).round() as u32).clamp(1, self.base.width() - ix as u32);
        let ih = ((f32::from(h) * sy).round() as u32).clamp(1, self.base.height() - iy as u32);
        (ix, iy, iw, ih)
    }

    fn crop_to(
        &self,
        window: &Window,
        selection: Option<(Pixels, Pixels, Pixels, Pixels)>,
    ) -> anyhow::Result<Pixmap> {
        let Some(sel) = selection else {
            return Ok((*self.base).clone());
        };
        let (ix, iy, iw, ih) = self.selection_to_image(window, sel);
        Ok(ashot_core::render::crop(&self.base, ix, iy, iw, ih)?)
    }

    /// Record purpose: hand the region to the recording flow and get out of
    /// the way (the overlay must not be on screen while recording).
    fn start_record(
        &mut self,
        selection: Option<(Pixels, Pixels, Pixels, Pixels)>,
        height: u32,
        mic: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let crop = selection.map(|sel| {
            let (x, y, w, h) = self.selection_to_image(window, sel);
            (x.max(0) as u32, y.max(0) as u32, w, h)
        });
        window.remove_window();
        crate::launcher::start_recording_flow(crop, Some(height), mic, false, cx);
    }

    /// Capture → clipboard → preview card, in one step (window-space input).
    fn enter_preview(
        &mut self,
        selection: Option<(Pixels, Pixels, Pixels, Pixels)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rect = selection.map(|sel| self.selection_to_image(window, sel));
        self.enter_preview_rect(rect, cx);
    }

    /// Same, but the region is already in image pixels.
    fn enter_preview_rect(
        &mut self,
        rect: Option<(i32, i32, u32, u32)>,
        cx: &mut Context<Self>,
    ) {
        let pixmap = match rect {
            None => (*self.base).clone(),
            Some((x, y, w, h)) => match ashot_core::render::crop(&self.base, x, y, w, h) {
                Ok(p) => p,
                Err(e) => return fail(e.into(), cx),
            },
        };
        let copy_result = pixmap
            .encode_png()
            .map_err(|e| anyhow::anyhow!("png encode: {e}"))
            .and_then(|png| Ok(ashot_core::clipboard::copy_png(png)?));
        match copy_result {
            Ok(()) => println!("{}", serde_json::json!({ "ok": true, "action": "copy" })),
            Err(e) => eprintln!(
                "{}",
                serde_json::json!({ "ok": false, "error": { "code": "clipboard", "message": e.to_string() } })
            ),
        }
        self.preview = Some(PreviewState {
            thumb: to_render_image(&pixmap),
            pixmap,
        });
        self.phase = Phase::Preview;
        self.preview_hovered = false;
        self.dismiss_gen += 1;
        let gen = self.dismiss_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DISMISS_AFTER).await;
            let _ = this.update(cx, |this, cx| {
                if this.dismiss_gen == gen && this.phase == Phase::Preview && !this.preview_hovered
                {
                    cx.quit();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &self.preview else { return };
        let result = (|| -> anyhow::Result<std::path::PathBuf> {
            let path = ashot_core::paths::default_capture_path()?;
            let png = preview
                .pixmap
                .encode_png()
                .map_err(|e| anyhow::anyhow!("png encode: {e}"))?;
            std::fs::write(&path, png)?;
            Ok(path)
        })();
        match result {
            Ok(path) => {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "action": "save", "path": path.display().to_string() })
                );
                cx.quit();
            }
            Err(e) => fail(e, cx),
        }
    }

    fn edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.take() else { return };
        editor::open_window(preview.pixmap, None, cx);
        window.remove_window();
    }

    /// The capture was already auto-copied when the card appeared; the ⧉
    /// button just re-copies defensively (clipboard may have been overwritten
    /// since) and closes. No second "copy" log line — one capture, one event.
    fn copy_again(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &self.preview else { return };
        let result = preview
            .pixmap
            .encode_png()
            .map_err(|e| anyhow::anyhow!("png encode: {e}"))
            .and_then(|png| Ok(ashot_core::clipboard::copy_png(png)?));
        match result {
            Ok(()) => cx.quit(),
            Err(e) => fail(e, cx),
        }
    }

    /// Minimal thumbnail card; hovering reveals ✕ / ⧉ / ✎ centered over it.
    fn preview_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.preview.as_ref().expect("card rendered only in Preview");
        let (iw, ih) = (preview.pixmap.width() as f32, preview.pixmap.height() as f32);
        let scale = (300.0 / iw).min(180.0 / ih).min(1.0);
        let (tw, th) = ((iw * scale).max(60.0), (ih * scale).max(40.0));

        let icon = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .w(px(34.))
                .h(px(34.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor(CursorStyle::PointingHand)
                .bg(theme::surface())
                .border_1()
                .border_color(theme::border())
                .text_sm()
                .text_color(theme::text())
                .hover(|s| s.bg(theme::accent()).text_color(gpui::rgb(0xffffff)))
                .child(glyph)
        };

        div()
            .id("preview-card")
            .absolute()
            .left(px(16.))
            .bottom(px(16.))
            .p_1()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.preview_hovered = *hovered;
                cx.notify();
            }))
            .child(
                div()
                    .relative()
                    .child(img(preview.thumb.clone()).w(px(tw)).h(px(th)).rounded_md())
                    .when(self.preview_hovered, |d| {
                        d.child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded_md()
                                .bg(theme::scrim())
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap_2()
                                .child(icon("close", "✕").on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|_, _: &MouseDownEvent, _, cx| cx.quit()),
                                ))
                                .child(icon("copy", "⧉").on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                        this.copy_again(cx)
                                    }),
                                ))
                                .child(icon("edit", "✎").on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                        this.edit(window, cx)
                                    }),
                                )),
                        )
                    }),
            )
    }
}

fn fail(err: anyhow::Error, cx: &mut App) {
    eprintln!(
        "{}",
        serde_json::json!({ "ok": false, "error": { "code": "app", "message": err.to_string() } })
    );
    cx.quit();
}

impl Focusable for OverlayView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OverlayView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();

        // Hidden test hook: ASHOT_TEST_PREVIEW=1 jumps straight to the
        // preview card with a fixed region, so the flow can be exercised
        // without input injection (used by automated visual checks).
        if let Some(rect) = self.start_preview.take() {
            if self.phase == Phase::Idle {
                self.enter_preview_rect(rect, cx);
            }
        }
        if self.phase == Phase::Idle && self.dismiss_gen == 0 {
            if let Some(val) = std::env::var_os("ASHOT_TEST_PREVIEW") {
                self.enter_preview(Some((px(200.), px(150.), px(500.), px(300.))), window, cx);
                if val == "hover" {
                    self.preview_hovered = true;
                }
            }
        }

        let mut root = div()
            .id("overlay")
            .relative()
            .size_full()
            .bg(theme::bg())
            .cursor(if self.phase == Phase::Preview {
                CursorStyle::Arrow
            } else {
                CursorStyle::Crosshair
            })
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match (this.phase, ev.keystroke.key.as_str()) {
                    (_, "escape") => cx.quit(),
                    (Phase::Preview, "enter") | (Phase::Preview, "s") => this.save(cx),
                    (Phase::Preview, "e") => this.edit(window, cx),
                    (Phase::Idle, "enter") => match this.purpose.clone() {
                        Purpose::Screenshot => this.enter_preview(None, window, cx),
                        Purpose::Record { height, mic } => {
                            this.start_record(None, height, mic, window, cx)
                        }
                    },
                    _ => {}
                }
            }))
            .child(
                img(self.image.clone())
                    .absolute()
                    .left_0()
                    .top_0()
                    .w(viewport.width)
                    .h(viewport.height),
            );

        if self.phase != Phase::Preview {
            root = root
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.phase = Phase::Dragging;
                        this.start = ev.position;
                        this.end = ev.position;
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                    if this.phase == Phase::Dragging {
                        this.end = ev.position;
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                        if this.phase == Phase::Dragging {
                            this.end = ev.position;
                            match (this.selection(), this.purpose.clone()) {
                                (Some(sel), Purpose::Screenshot) => {
                                    this.enter_preview(Some(sel), window, cx)
                                }
                                (Some(sel), Purpose::Record { height, mic }) => {
                                    this.start_record(Some(sel), height, mic, window, cx)
                                }
                                (None, _) => {
                                    this.phase = Phase::Idle;
                                    cx.notify();
                                }
                            }
                        }
                    }),
                );
        }

        match self.phase {
            Phase::Preview => {
                // The frozen frame reads as the live desktop — no selection
                // leftovers, just the floating card.
                root = root.child(self.preview_card(cx));
            }
            _ => match self.selection() {
                None => {
                    root = root.child(div().absolute().inset_0().bg(theme::scrim())).child(
                        div()
                            .absolute()
                            .left(viewport.width / 2.0 - px(160.))
                            .top(px(24.))
                            .w(px(320.))
                            .flex()
                            .justify_center()
                            .py_2()
                            .rounded_lg()
                            .bg(theme::surface())
                            .border_1()
                            .border_color(theme::border())
                            .shadow_lg()
                            .text_sm()
                            .text_color(theme::text_muted())
                            .child(match self.purpose {
                                Purpose::Screenshot => {
                                    "Drag to select · Enter full screen · Esc cancel"
                                }
                                Purpose::Record { .. } => {
                                    "Drag the area to record · Enter full screen · Esc cancel"
                                }
                            }),
                    );
                }
                Some((x, y, w, h)) => {
                    let right_x = x + w;
                    let bottom_y = y + h;
                    root = root
                        .child(div().absolute().left_0().top_0().w(viewport.width).h(y).bg(theme::scrim()))
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top(bottom_y)
                                .w(viewport.width)
                                .h(viewport.height - bottom_y)
                                .bg(theme::scrim()),
                        )
                        .child(div().absolute().left_0().top(y).w(x).h(h).bg(theme::scrim()))
                        .child(
                            div()
                                .absolute()
                                .left(right_x)
                                .top(y)
                                .w(viewport.width - right_x)
                                .h(h)
                                .bg(theme::scrim()),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(x)
                                .top(y)
                                .w(w)
                                .h(h)
                                .border_2()
                                .border_color(theme::accent())
                                .bg(theme::accent_fill()),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(x)
                                .top(if bottom_y + px(30.) < viewport.height {
                                    bottom_y + px(6.)
                                } else {
                                    y - px(28.)
                                })
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .bg(theme::surface())
                                .border_1()
                                .border_color(theme::border())
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child({
                                    let viewport_w = f32::from(viewport.width).max(1.0);
                                    let scale = self.base.width() as f32 / viewport_w;
                                    format!(
                                        "{} × {}",
                                        (f32::from(w) * scale).round() as u32,
                                        (f32::from(h) * scale).round() as u32
                                    )
                                }),
                        );
                }
            },
        }
        root
    }
}
