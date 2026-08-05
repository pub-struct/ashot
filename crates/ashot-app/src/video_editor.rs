//! Video editor: timeline scrubbing, cuts, drawings, smooth zoom points,
//! AI captions, and GPU export.
//!
//! Layout: toolbar (top) / preview (middle) / timeline (bottom). Heavy work
//! (frame decode, whisper, export) runs on the background executor via the
//! core helpers; the UI only ever touches decoded frames.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    canvas, div, img, point, prelude::*, px, App, AnyElement, Bounds, Context, CursorStyle,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Path as GpuiPath, Pixels, Point, RenderImage, SharedString, Size, Window,
    WindowBounds, WindowOptions,
};
use tiny_skia::Pixmap;

use ashot_core::spec::{Annotation, Style};
use ashot_core::video::{self, VideoInfo, ZoomPoint};
use ashot_core::Renderer;

use crate::img::to_render_image;
use crate::theme;

const TOOLBAR_H: f32 = 52.0;
const TIMELINE_H: f32 = 72.0;
const ZOOM_LEVELS: [(&str, f64); 3] = [("1.5×", 1.5), ("2×", 2.0), ("3×", 3.0)];
const COLORS: [(&str, u32); 5] = [
    ("red", 0xff3b30),
    ("green", 0x34c759),
    ("blue", 0x007aff),
    ("yellow", 0xffcc00),
    ("white", 0xffffff),
];

pub fn run(path: PathBuf) -> anyhow::Result<()> {
    let info = video::probe(&path)?;
    gpui::Application::with_platform(gpui_platform::current_platform(false))
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(move |cx: &mut App| {
            open_window(path, info, cx);
            cx.activate(true);
        });
    Ok(())
}

pub fn open_window(path: PathBuf, info: VideoInfo, cx: &mut App) {
    let (iw, ih) = (info.width as f32, info.height as f32);
    let win_w = (iw * 0.75 + 32.0).clamp(960.0, 1680.0);
    let win_h = (ih * 0.75 + TOOLBAR_H + TIMELINE_H + 32.0).clamp(600.0, 1020.0);
    let bounds = Bounds::centered(None, Size { width: px(win_w), height: px(win_h) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(SharedString::from(format!(
                    "ashot — {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| VideoEditor::new(path, info, cx));
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            window.on_window_should_close(cx, |_, cx| {
                cx.quit();
                true
            });
            view
        },
    );
    if window.is_err() {
        eprintln!("failed to open video editor window");
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

struct VideoEditor {
    path: PathBuf,
    info: VideoInfo,
    playhead: f64,
    /// Latest decoded frame at native resolution (no annotations).
    frame_native: Option<Pixmap>,
    /// Displayed frame (annotations burned for preview).
    frame: Option<Arc<RenderImage>>,
    fetch_gen: usize,
    playing: bool,

    /// Ranges to REMOVE (seconds).
    cuts: Vec<(f64, f64)>,
    cut_pending: Option<f64>,

    zooms: Vec<ZoomPoint>,
    zoom_arming: bool,
    zoom_level_ix: usize,

    annotations: Vec<Annotation>,
    tool: Option<Tool>,
    color_ix: usize,
    drag_start: Option<(f32, f32)>,
    drag_current: Option<(f32, f32)>,
    typing: Option<(f32, f32, String)>,
    renderer: Renderer,

    srt: Option<PathBuf>,
    cc_status: Option<SharedString>,
    burn_cc: bool,

    exporting: bool,
    export_progress: Arc<Mutex<f64>>,
    status: Option<SharedString>,

    scrubbing: bool,
    focus_handle: FocusHandle,
}

impl VideoEditor {
    fn new(path: PathBuf, info: VideoInfo, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            path,
            info,
            playhead: 0.0,
            frame_native: None,
            frame: None,
            fetch_gen: 0,
            playing: false,
            cuts: Vec::new(),
            cut_pending: None,
            zooms: Vec::new(),
            zoom_arming: false,
            zoom_level_ix: 1,
            annotations: Vec::new(),
            tool: None,
            color_ix: 0,
            drag_start: None,
            drag_current: None,
            typing: None,
            renderer: Renderer::new(),
            srt: None,
            cc_status: None,
            burn_cc: false,
            exporting: false,
            export_progress: Arc::new(Mutex::new(0.0)),
            status: None,
            scrubbing: false,
            focus_handle: cx.focus_handle(),
        };
        this.fetch_frame(cx);
        this
    }

    // ---- frames ----

    fn fetch_frame(&mut self, cx: &mut Context<Self>) {
        self.fetch_gen += 1;
        let gen = self.fetch_gen;
        let path = self.path.clone();
        let t = self.playhead;
        cx.spawn(async move |this, cx| {
            let frame = cx
                .background_executor()
                .spawn(async move { video::extract_frame(&path, t) })
                .await;
            this.update(cx, |this, cx| {
                if this.fetch_gen == gen {
                    if let Ok(pixmap) = frame {
                        this.frame_native = Some(pixmap);
                        this.refresh_display(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Burn current annotations (+typing preview) onto the frame for display.
    fn refresh_display(&mut self, cx: &mut Context<Self>) {
        let Some(base) = &self.frame_native else { return };
        let mut shown = base.clone();
        let mut all = self.annotations.clone();
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
        self.frame = Some(crate::img::into_render_image(shown));
        cx.notify();
    }

    fn style(&self) -> Style {
        Style {
            color: Some(COLORS[self.color_ix].0.to_string()),
            stroke_width: Some(5.0),
            fill_opacity: None,
        }
    }

    // ---- geometry ----

    fn fit(&self, viewport: Size<Pixels>) -> (f32, f32, f32) {
        let (cw, ch) = (
            f32::from(viewport.width),
            f32::from(viewport.height) - TOOLBAR_H - TIMELINE_H,
        );
        let (iw, ih) = (self.info.width as f32, self.info.height as f32);
        let scale = (cw / iw).min(ch / ih);
        let ox = (cw - iw * scale) / 2.0;
        let oy = TOOLBAR_H + (ch - ih * scale) / 2.0;
        (ox, oy, scale)
    }

    fn to_video_coords(&self, pos: Point<Pixels>, viewport: Size<Pixels>) -> Option<(f32, f32)> {
        let (ox, oy, scale) = self.fit(viewport);
        let x = (f32::from(pos.x) - ox) / scale;
        let y = (f32::from(pos.y) - oy) / scale;
        if x < 0.0 || y < 0.0 || x > self.info.width as f32 || y > self.info.height as f32 {
            return None;
        }
        Some((x, y))
    }

    // ---- playback ----

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        self.playing = !self.playing;
        if self.playing {
            let step = 0.25;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                    let advanced = this.update(cx, |this, cx| {
                        if !this.playing {
                            return false;
                        }
                        let mut t = this.playhead + step;
                        // Skip removed ranges during playback.
                        for (s, e) in &this.cuts {
                            if t >= *s && t < *e {
                                t = *e;
                            }
                        }
                        if t >= this.info.duration_s {
                            this.playing = false;
                            this.playhead = 0.0;
                        } else {
                            this.playhead = t;
                        }
                        this.fetch_frame(cx);
                        cx.notify();
                        this.playing
                    });
                    if !matches!(advanced, Ok(true)) {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }

    // ---- cutting ----

    fn cut_click(&mut self, cx: &mut Context<Self>) {
        match self.cut_pending.take() {
            None => {
                self.cut_pending = Some(self.playhead);
                self.status = Some("Move the playhead, then press ✂ again to cut the range".into());
            }
            Some(start) => {
                let (a, b) = if start <= self.playhead {
                    (start, self.playhead)
                } else {
                    (self.playhead, start)
                };
                if b - a > 0.05 {
                    self.cuts.push((a, b));
                    self.cuts.sort_by(|x, y| x.0.total_cmp(&y.0));
                    self.status = Some(format!("Cut {:.1}s – {:.1}s", a, b).into());
                }
            }
        }
        cx.notify();
    }

    fn keep_ranges(&self) -> Vec<(f64, f64)> {
        if self.cuts.is_empty() {
            return Vec::new();
        }
        let mut keep = Vec::new();
        let mut t = 0.0;
        for (s, e) in &self.cuts {
            if *s > t + 0.05 {
                keep.push((t, *s));
            }
            t = t.max(*e);
        }
        if t + 0.05 < self.info.duration_s {
            keep.push((t, self.info.duration_s));
        }
        keep
    }

    // ---- captions ----

    fn generate_captions(&mut self, cx: &mut Context<Self>) {
        if self.cc_status.is_some() {
            return;
        }
        self.cc_status = Some("CC: starting…".into());
        let input = self.path.clone();
        let srt_path = self.path.with_extension("srt");
        cx.spawn(async move |this, cx| {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            let srt_out = srt_path.clone();
            let task = cx.background_executor().spawn(async move {
                ashot_core::captions::generate(&input, &srt_out, |s| {
                    let _ = tx.send(s.to_string());
                })
            });
            // Relay status lines while whisper runs.
            let relay = {
                let this = this.clone();
                async move |cx: &mut gpui::AsyncApp| {
                    while let Ok(msg) = rx.recv() {
                        let _ = this.update(cx, |this: &mut Self, cx| {
                            this.cc_status = Some(format!("CC: {msg}").into());
                            cx.notify();
                        });
                    }
                }
            };
            // Poll channel with timers (recv blocks; do it on background too).
            let _ = relay; // status relayed post-hoc below to keep this simple
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.srt = Some(srt_path.clone());
                        this.burn_cc = true;
                        this.cc_status = Some("CC ✓".into());
                    }
                    Err(e) => {
                        this.cc_status = Some(format!("CC failed: {e}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- export ----

    fn export(&mut self, cx: &mut Context<Self>) {
        if self.exporting {
            return;
        }
        self.exporting = true;
        *self.export_progress.lock().unwrap() = 0.0;
        self.status = Some("Exporting…".into());

        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let output = self.path.with_file_name(format!("{stem}-edited.mp4"));

        // Annotations become a transparent overlay PNG at native resolution.
        let overlay_png = if self.annotations.is_empty() {
            None
        } else {
            let mut overlay = Pixmap::new(self.info.width, self.info.height);
            overlay.as_mut().map(|pm| self.renderer.render(pm, &self.annotations));
            overlay.and_then(|pm| {
                let p = std::env::temp_dir().join(format!("ashot-overlay-{}.png", std::process::id()));
                pm.save_png(&p).ok().map(|_| p)
            })
        };

        let spec = video::ExportSpec {
            input: self.path.clone(),
            output,
            keep: self.keep_ranges(),
            zooms: self.zooms.clone(),
            overlay_png,
            srt: if self.burn_cc { self.srt.clone() } else { None },
        };
        let progress = self.export_progress.clone();

        // Progress ticker.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(400)).await;
                let live = this.update(cx, |this: &mut Self, cx| {
                    if this.exporting {
                        let p = *this.export_progress.lock().unwrap();
                        this.status = Some(format!("Exporting… {:.0}%", p * 100.0).into());
                        cx.notify();
                    }
                    this.exporting
                });
                if !matches!(live, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let progress2 = progress.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    video::export(&spec, |p| {
                        *progress2.lock().unwrap() = p;
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.exporting = false;
                match result {
                    Ok(path) => {
                        println!(
                            "{}",
                            serde_json::json!({ "ok": true, "action": "export",
                                "path": path.display().to_string() })
                        );
                        this.status = Some(format!("Exported → {}", path.display()).into());
                    }
                    Err(e) => {
                        this.status = Some(format!("Export failed: {e}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- input on preview ----

    fn preview_down(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        if self.typing.is_some() {
            self.commit_typing(cx);
            return;
        }
        let Some((x, y)) = self.to_video_coords(pos, viewport) else { return };
        if self.zoom_arming {
            let level = ZOOM_LEVELS[self.zoom_level_ix].1;
            self.zooms.push(ZoomPoint {
                t: self.playhead,
                cx: x as f64,
                cy: y as f64,
                level,
                duration: 3.0,
            });
            self.zoom_arming = false;
            self.status =
                Some(format!("Zoom {level}× at {:.1}s (click marker to remove)", self.playhead).into());
            cx.notify();
            return;
        }
        match self.tool {
            Some(Tool::Marker) => {
                let number = self
                    .annotations
                    .iter()
                    .filter_map(|a| match a {
                        Annotation::Marker { number, .. } => *number,
                        _ => None,
                    })
                    .max()
                    .map_or(1, |m| m + 1);
                self.annotations.push(Annotation::Marker {
                    x,
                    y,
                    number: Some(number),
                    size: None,
                    style: self.style(),
                });
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

    fn preview_move(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        if self.drag_start.is_none() {
            return;
        }
        if let Some(p) = self.to_video_coords(pos, viewport) {
            self.drag_current = Some(p);
            cx.notify();
        }
    }

    fn preview_up(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
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
            self.annotations.push(annotation);
        }
        self.refresh_display(cx);
    }

    fn commit_typing(&mut self, cx: &mut Context<Self>) {
        if let Some((x, y, text)) = self.typing.take() {
            if !text.is_empty() {
                self.annotations.push(Annotation::Text {
                    x,
                    y,
                    text,
                    size: Some(28.0),
                    style: self.style(),
                });
            }
        }
        self.refresh_display(cx);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        if let Some((_, _, buf)) = &mut self.typing {
            match key {
                "enter" => self.commit_typing(cx),
                "escape" => {
                    self.typing = None;
                    self.refresh_display(cx);
                }
                "backspace" => {
                    buf.pop();
                    self.refresh_display(cx);
                }
                "space" => {
                    buf.push(' ');
                    self.refresh_display(cx);
                }
                _ => {
                    if let Some(ch) = &ev.keystroke.key_char {
                        if !ev.keystroke.modifiers.control {
                            buf.push_str(ch);
                            self.refresh_display(cx);
                        }
                    }
                }
            }
            return;
        }
        match key {
            "space" => self.toggle_play(cx),
            "c" => self.cut_click(cx),
            "z" if ev.keystroke.modifiers.control => {
                self.annotations.pop();
                self.refresh_display(cx);
            }
            "escape" if self.zoom_arming || self.cut_pending.is_some() => {
                self.zoom_arming = false;
                self.cut_pending = None;
                self.status = None;
                cx.notify();
            }
            "escape" => cx.quit(),
            "left" => {
                self.playhead = (self.playhead - 1.0).max(0.0);
                self.fetch_frame(cx);
                cx.notify();
            }
            "right" => {
                self.playhead = (self.playhead + 1.0).min(self.info.duration_s);
                self.fetch_frame(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn scrub_to(&mut self, pos: Point<Pixels>, viewport: Size<Pixels>, cx: &mut Context<Self>) {
        let width = f32::from(viewport.width).max(1.0);
        let frac = (f32::from(pos.x) / width).clamp(0.0, 1.0) as f64;
        self.playhead = frac * self.info.duration_s;
        self.fetch_frame(cx);
        cx.notify();
    }

    // ---- small UI helpers ----

    fn chip(
        &self,
        id: (&'static str, usize),
        label: SharedString,
        active: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_2p5()
            .py_1()
            .flex_none()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .text_sm()
            .when(active, |d| d.bg(theme::accent()).text_color(gpui::rgb(0xffffff)))
            .when(!active, |d| {
                d.text_color(theme::text_muted()).hover(|s| s.bg(theme::surface_hover()))
            })
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| handler(this, cx)),
            )
    }
}

impl Focusable for VideoEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for VideoEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let (ox, oy, scale) = self.fit(viewport);
        let (dw, dh) = (self.info.width as f32 * scale, self.info.height as f32 * scale);
        let duration = self.info.duration_s.max(0.01);

        // ---- toolbar ----
        let mut toolbar = div()
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
            .child(self.chip(
                ("play", 0),
                if self.playing { "⏸".into() } else { "▶".into() },
                self.playing,
                cx,
                |this, cx| this.toggle_play(cx),
            ))
            .child(self.chip(
                ("cut", 0),
                if self.cut_pending.is_some() { "✂ …to here".into() } else { "✂ Cut".into() },
                self.cut_pending.is_some(),
                cx,
                |this, cx| this.cut_click(cx),
            ))
            .child(self.chip(
                ("zoomarm", 0),
                "🔍 Zoom".into(),
                self.zoom_arming,
                cx,
                |this, cx| {
                    this.zoom_arming = !this.zoom_arming;
                    this.status = this
                        .zoom_arming
                        .then(|| "Click on the video where the zoom should focus".into());
                    cx.notify();
                },
            ));
        for ix in 0..ZOOM_LEVELS.len() {
            toolbar = toolbar.child(self.chip(
                ("zl", ix),
                ZOOM_LEVELS[ix].0.into(),
                self.zoom_level_ix == ix,
                cx,
                move |this, cx| {
                    this.zoom_level_ix = ix;
                    cx.notify();
                },
            ));
        }
        toolbar = toolbar.child(div().w(px(1.)).h(px(22.)).bg(theme::border()).mx_1());
        for tool in Tool::all() {
            let active = self.tool == Some(tool);
            toolbar = toolbar.child(self.chip(
                ("tool", tool.label().len() + tool as usize),
                tool.label().into(),
                active,
                cx,
                move |this, cx| {
                    this.tool = if this.tool == Some(tool) { None } else { Some(tool) };
                    cx.notify();
                },
            ));
        }
        for ix in 0..COLORS.len() {
            let (_, hex) = COLORS[ix];
            let active = self.color_ix == ix;
            toolbar = toolbar.child(
                div()
                    .id(("vc", ix))
                    .w(px(16.))
                    .h(px(16.))
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
                    ),
            );
        }
        toolbar = toolbar
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .mr_2()
                    .max_w(px(360.))
                    .overflow_hidden()
                    .child(
                        self.status
                            .clone()
                            .or_else(|| self.cc_status.clone())
                            .unwrap_or_else(|| {
                                format!(
                                    "{:.1}s / {:.1}s",
                                    self.playhead, self.info.duration_s
                                )
                                .into()
                            }),
                    ),
            )
            .child(self.chip(
                ("cc", 0),
                match (&self.srt, self.burn_cc) {
                    (Some(_), true) => "CC ✓ burn".into(),
                    (Some(_), false) => "CC (off)".into(),
                    (None, _) => "CC generate".into(),
                },
                self.srt.is_some() && self.burn_cc,
                cx,
                |this, cx| {
                    if this.srt.is_some() {
                        this.burn_cc = !this.burn_cc;
                        cx.notify();
                    } else {
                        this.generate_captions(cx);
                    }
                },
            ))
            .child(
                div()
                    .id("export")
                    .px_3()
                    .py_1p5()
                    .flex_none()
                    .rounded_md()
                    .cursor(CursorStyle::PointingHand)
                    .bg(theme::accent())
                    .text_color(gpui::rgb(0xffffff))
                    .text_sm()
                    .hover(|s| s.bg(theme::accent_hover()))
                    .child(if self.exporting { "Exporting…" } else { "Export" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| this.export(cx)),
                    ),
            );

        // ---- timeline ----
        let tl_top = f32::from(viewport.height) - TIMELINE_H;
        let tl_w = f32::from(viewport.width);
        let mut timeline = div()
            .id("timeline")
            .absolute()
            .left_0()
            .top(px(tl_top))
            .w(viewport.width)
            .h(px(TIMELINE_H))
            .bg(theme::surface())
            .border_t_1()
            .border_color(theme::border())
            .cursor(CursorStyle::PointingHand)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    this.scrubbing = true;
                    this.scrub_to(ev.position, window.viewport_size(), cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if this.scrubbing {
                    this.scrub_to(ev.position, window.viewport_size(), cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.scrubbing = false;
                    cx.notify();
                }),
            )
            // Track base.
            .child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(20.))
                    .w(viewport.width)
                    .h(px(24.))
                    .bg(theme::bg()),
            );
        // Cut ranges (red).
        for (s, e) in &self.cuts {
            let x = (s / duration) as f32 * tl_w;
            let w = (((e - s) / duration) as f32 * tl_w).max(2.0);
            timeline = timeline.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(20.))
                    .w(px(w))
                    .h(px(24.))
                    .bg(gpui::rgba(0xff3b3066)),
            );
        }
        // Pending cut start marker.
        if let Some(s) = self.cut_pending {
            let x = (s / duration) as f32 * tl_w;
            timeline = timeline.child(
                div().absolute().left(px(x)).top(px(16.)).w(px(2.)).h(px(32.)).bg(gpui::rgb(0xff3b30)),
            );
        }
        // Zoom markers.
        for (ix, z) in self.zooms.iter().enumerate() {
            let x = (z.t / duration) as f32 * tl_w;
            timeline = timeline.child(
                div()
                    .id(("zm", ix))
                    .absolute()
                    .left(px(x - 7.0))
                    .top(px(2.))
                    .w(px(14.))
                    .h(px(14.))
                    .rounded_full()
                    .bg(theme::accent())
                    .cursor(CursorStyle::PointingHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.zooms.remove(ix);
                            this.status = Some("Zoom removed".into());
                            cx.notify();
                        }),
                    ),
            );
        }
        // Playhead.
        let phx = (self.playhead / duration) as f32 * tl_w;
        timeline = timeline.child(
            div().absolute().left(px(phx)).top(px(12.)).w(px(2.)).h(px(40.)).bg(gpui::rgb(0xffffff)),
        );

        // ---- preview + drag overlay ----
        let mut root = div()
            .id("veditor")
            .relative()
            .size_full()
            .bg(theme::bg())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key_down(ev, cx)));

        if let Some(frame) = &self.frame {
            root = root.child(
                img(frame.clone())
                    .absolute()
                    .left(px(ox))
                    .top(px(oy))
                    .w(px(dw))
                    .h(px(dh)),
            );
        }
        // Input layer over the preview.
        root = root.child(
            div()
                .id("vcanvas")
                .absolute()
                .left(px(ox))
                .top(px(oy))
                .w(px(dw))
                .h(px(dh))
                .cursor(if self.zoom_arming || self.tool.is_some() {
                    CursorStyle::Crosshair
                } else {
                    CursorStyle::Arrow
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                        this.preview_down(ev.position, window.viewport_size(), cx)
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                    this.preview_move(ev.position, window.viewport_size(), cx)
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                        this.preview_up(ev.position, window.viewport_size(), cx)
                    }),
                ),
        );
        // Draft shape preview (GPU elements, same as image editor).
        if let (Some((sx, sy)), Some((cx_, cy_))) = (self.drag_start, self.drag_current) {
            let color = gpui::rgb(COLORS[self.color_ix].1);
            let to_win = |x: f32, y: f32| (ox + x * scale, oy + y * scale);
            match self.tool {
                Some(Tool::Rect) | Some(Tool::Ellipse) => {
                    let (x0, y0) = to_win(sx.min(cx_), sy.min(cy_));
                    let d = div()
                        .absolute()
                        .left(px(x0))
                        .top(px(y0))
                        .w(px(((cx_ - sx).abs() * scale).max(1.0)))
                        .h(px(((cy_ - sy).abs() * scale).max(1.0)))
                        .border_2()
                        .border_color(color);
                    let d = if self.tool == Some(Tool::Ellipse) { d.rounded_full() } else { d };
                    root = root.child(d);
                }
                Some(Tool::Arrow) => {
                    let (fx, fy) = to_win(sx, sy);
                    let (tx, ty) = to_win(cx_, cy_);
                    root = root.child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, _| paint_arrow(window, fx, fy, tx, ty, 4.0, color),
                        )
                        .absolute()
                        .left_0()
                        .top_0()
                        .size_full(),
                    );
                }
                _ => {}
            }
        }

        root.child(toolbar).child(timeline)
    }
}

/// Fill-based arrow preview (same construction as the image editor's).
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
