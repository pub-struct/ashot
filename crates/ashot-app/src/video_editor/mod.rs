//! Video editor: timeline scrubbing, cuts, drawings, smooth zoom points,
//! AI captions, and GPU export.
//!
//! Layout: toolbar (top) / preview (middle) / timeline (bottom). Preview
//! frames stream from a persistent GStreamer player process
//! (`ashot_core::player`) at source framerate; other heavy work (whisper,
//! export) runs on the background executor via the core helpers. The UI only
//! ever touches decoded frames.

mod inspector;
mod preview;
mod state;
mod timeline;
mod undo;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    canvas, div, img, prelude::*, px, App, Bounds, Context, CursorStyle,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, RenderImage, SharedString, Size, Window,
    WindowBounds, WindowOptions,
};
use tiny_skia::Pixmap;

use ashot_core::player::{PlayerEvent, PreviewPlayer};
use ashot_core::spec::Style;
use ashot_core::video::{self, VideoInfo};
use ashot_core::Renderer;

use crate::theme;

use self::inspector::{render_inspector, INSPECTOR_W};
use self::preview::paint_arrow;
use self::state::EditState;
use self::timeline::TimelineDrag;
use self::undo::History;

const TOOLBAR_H: f32 = 52.0;
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
    let win_w = (iw * 0.75 + INSPECTOR_W + 32.0).clamp(1100.0, 1800.0);
    let win_h = (ih * 0.75 + TOOLBAR_H + timeline::TIMELINE_H + 32.0).clamp(680.0, 1080.0);
    let bounds = Bounds::centered(None, Size { width: px(win_w), height: px(win_h) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(Size { width: px(900.0), height: px(560.0) }),
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
    /// Persistent streaming decoder (`None` if it failed to spawn).
    player: Option<PreviewPlayer>,
    /// Frames/EOS/errors from the player's reader thread, drained by the
    /// pump task spawned in `new`.
    player_rx: std::sync::mpsc::Receiver<PlayerEvent>,
    /// Latest streamed frame, preview-scaled (no annotations).
    frame: Option<Arc<RenderImage>>,
    /// Annotations (+ typing caret) burned onto a transparent native-res
    /// image, layered over `frame` at render time. Rebuilt only when
    /// annotations change, never per frame.
    overlay_frame: Option<Arc<RenderImage>>,
    playing: bool,

    /// Cuts, zoom points, annotations, caption-burn toggle — the undoable
    /// edit state. See `state::EditState`.
    state: EditState,
    /// Snapshot-based undo/redo stack over `state`. See `undo::History`.
    history: History,
    /// Currently-selected timeline item (unused this stage beyond being
    /// present — Stages 2/3 wire real selection UX).
    selection: state::Selection,

    zoom_level_ix: usize,

    tool: Option<Tool>,
    color_ix: usize,
    drag_start: Option<(f32, f32)>,
    drag_current: Option<(f32, f32)>,
    typing: Option<(f32, f32, String)>,
    renderer: Renderer,

    srt: Option<PathBuf>,
    cc_status: Option<SharedString>,
    /// Set while a caption-generation task is in flight; drives the status
    /// ticker below and stops it once the task completes.
    cc_running: bool,
    /// Latest progress line from `ashot_core::captions::generate`, written
    /// from the background task and polled by the ticker in
    /// `generate_captions`.
    cc_progress: Arc<Mutex<String>>,

    exporting: bool,
    export_progress: Arc<Mutex<f64>>,
    status: Option<SharedString>,

    scrubbing: bool,
    /// In-flight timeline drag (segment edge / zoom body / zoom edge). See
    /// `timeline::TimelineDrag`; commits to `history` once, on mouse-up.
    timeline_drag: Option<TimelineDrag>,
    /// In-flight crop-rect drag on the preview (move center / resize level)
    /// for the selected zoom segment. See `preview::PreviewDrag`.
    preview_drag: Option<preview::PreviewDrag>,

    /// Video-lane thumbnail strip; `None` while the background extraction is
    /// in flight (see `load_timeline_media`).
    thumbnails: Option<Vec<(f64, Arc<RenderImage>)>>,
    /// Audio-lane waveform peaks; `None` while loading, `Some(empty)` if the
    /// source has no audio track.
    peaks: Option<Arc<Vec<(f32, f32)>>>,
    /// Captions-lane cues, parsed from `srt`; empty until captions exist.
    cues: Vec<ashot_core::srt::Cue>,
    /// Guards the one-shot background media load against stale results.
    media_gen: usize,

    focus_handle: FocusHandle,
}

impl VideoEditor {
    fn new(path: PathBuf, info: VideoInfo, cx: &mut Context<Self>) -> Self {
        let state = EditState::new(info.duration_s);
        let history = History::new(state.clone());
        // Stream at native resolution: the preview must stay pixel-true and
        // zoom (a layout transform, up to 8×) magnifies the texture, so any
        // downscale here reads as blur. Native 1080p60 RGBA over the pipe is
        // well within budget, and appsink drops frames if the UI falls behind.
        let (player_tx, player_rx) = std::sync::mpsc::channel();
        let player = match PreviewPlayer::spawn(&path, 0, player_tx) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("preview player failed: {e}");
                None
            }
        };
        let mut this = Self {
            path,
            info,
            playhead: 0.0,
            player,
            player_rx,
            frame: None,
            overlay_frame: None,
            playing: false,
            state,
            history,
            selection: state::Selection::None,
            zoom_level_ix: 1,
            tool: None,
            color_ix: 0,
            drag_start: None,
            drag_current: None,
            typing: None,
            renderer: Renderer::new(),
            srt: None,
            cc_status: None,
            cc_running: false,
            cc_progress: Arc::new(Mutex::new(String::new())),
            exporting: false,
            export_progress: Arc::new(Mutex::new(0.0)),
            status: None,
            scrubbing: false,
            timeline_drag: None,
            preview_drag: None,
            thumbnails: None,
            peaks: None,
            cues: Vec::new(),
            media_gen: 0,
            focus_handle: cx.focus_handle(),
        };
        // If a same-named .srt sidecar already exists from a prior session,
        // pick it up so the captions lane isn't empty on reopen.
        let sidecar = this.path.with_extension("srt");
        if sidecar.exists() {
            this.srt = Some(sidecar);
        }
        if this.player.is_none() {
            this.status = Some("Preview unavailable: player failed to start".into());
        }
        this.spawn_player_pump(cx);
        this.load_timeline_media(cx);
        this.reload_cues(cx);
        this
    }

    // ---- background media (thumbnails / waveform / caption cues) ----

    /// Fires once: extracts thumbnails + waveform peaks in the background and
    /// caches them (keyed by canonicalized path) so reopening the same video
    /// is instant. See `ashot-app/src/timeline_media.rs`.
    fn load_timeline_media(&mut self, cx: &mut Context<Self>) {
        if let Some(cached) = crate::timeline_media::cached(&self.path) {
            self.thumbnails = Some(cached.thumbnails.clone());
            self.peaks = Some(cached.peaks.clone());
            cx.notify();
            return;
        }
        self.media_gen += 1;
        let gen = self.media_gen;
        let path = self.path.clone();
        let duration = self.info.duration_s;
        cx.spawn(async move |this, cx| {
            let (path, thumbs, peaks) = cx
                .background_executor()
                .spawn(async move {
                    let thumbs = ashot_core::thumbnails::extract_thumbnails(&path, duration, 24, 90)
                        .unwrap_or_default();
                    let peaks = ashot_core::audio::extract_peaks(&path, 1500);
                    (path, thumbs, peaks)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.media_gen != gen {
                    return;
                }
                let images: Vec<(f64, Arc<RenderImage>)> = thumbs
                    .into_iter()
                    .map(|t| (t.t, crate::img::into_render_image(t.pixmap)))
                    .collect();
                let peaks = Arc::new(peaks.peaks);
                crate::timeline_media::insert(
                    &path,
                    Arc::new(crate::timeline_media::TimelineMedia {
                        thumbnails: images.clone(),
                        peaks: peaks.clone(),
                    }),
                );
                this.thumbnails = Some(images);
                this.peaks = Some(peaks);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-derive the captions lane from `self.srt`. Called on startup (if an
    /// SRT sidecar already exists) and again after `generate_captions`
    /// succeeds, since cues can change mid-session.
    fn reload_cues(&mut self, cx: &mut Context<Self>) {
        let Some(srt) = self.srt.clone() else {
            self.cues.clear();
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let cues = cx
                .background_executor()
                .spawn(async move { ashot_core::srt::parse_srt(&srt).unwrap_or_default() })
                .await;
            this.update(cx, |this, cx| {
                this.cues = cues;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- frames ----

    /// Show the frame at the playhead: flush-seek the persistent player.
    /// While paused the pipeline prerolls and emits exactly that frame;
    /// while playing, playback continues from the new position. The frame
    /// itself arrives asynchronously via the pump.
    fn fetch_frame(&mut self, _cx: &mut Context<Self>) {
        if let Some(p) = self.player.as_mut() {
            p.seek(self.playhead);
        }
    }

    /// Drain player events onto the entity ~60×/s. A timer-driven pump (like
    /// the export/CC progress tickers) keeps the mpsc receiver off the async
    /// executor; draining keeps only the newest frame so a slow UI never
    /// falls behind the stream.
    fn spawn_player_pump(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                if this.update(cx, |this, cx| this.pump_player(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn pump_player(&mut self, cx: &mut Context<Self>) {
        let mut latest: Option<(f64, Pixmap)> = None;
        let mut eos = false;
        let mut error: Option<String> = None;
        while let Ok(ev) = self.player_rx.try_recv() {
            match ev {
                PlayerEvent::Frame { t, pixmap } => latest = Some((t, pixmap)),
                PlayerEvent::Eos => eos = true,
                PlayerEvent::Error(e) => error = Some(e),
            }
        }
        if let Some((t, pixmap)) = latest {
            self.frame = Some(crate::img::into_render_image(pixmap));
            if self.playing {
                // The playhead follows the stream during playback; removed
                // segments are skipped by seeking over them (adjacent removed
                // segments in one hop).
                let mut target = t;
                while let Some(seg) = self
                    .state
                    .segments
                    .iter()
                    .find(|s| s.removed && target >= s.start && target < s.end)
                {
                    target = seg.end;
                }
                if target >= self.info.duration_s {
                    eos = true;
                } else {
                    self.playhead = target;
                    if target != t {
                        if let Some(p) = self.player.as_mut() {
                            p.seek(target);
                        }
                    }
                }
            }
            cx.notify();
        }
        if eos {
            self.playing = false;
            self.playhead = 0.0;
            if let Some(p) = self.player.as_mut() {
                p.pause();
                p.seek(0.0);
            }
            cx.notify();
        }
        if let Some(e) = error {
            self.status = Some(format!("Preview error: {e}").into());
            cx.notify();
        }
    }

    fn style(&self) -> Style {
        Style {
            color: Some(COLORS[self.color_ix].0.to_string()),
            stroke_width: Some(5.0),
            fill_opacity: None,
        }
    }

    // ---- playback ----

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let Some(_) = self.player.as_ref() else { return };
        self.playing = !self.playing;
        if self.playing {
            // Restart from 0 at the end; never start inside a removed range.
            let mut t = if self.playhead >= self.info.duration_s - 0.05 { 0.0 } else { self.playhead };
            while let Some(seg) =
                self.state.segments.iter().find(|s| s.removed && t >= s.start && t < s.end)
            {
                t = seg.end;
            }
            if t >= self.info.duration_s {
                t = 0.0;
            }
            let p = self.player.as_mut().unwrap();
            if t != self.playhead {
                self.playhead = t;
                p.seek(t);
            }
            p.play();
        } else if let Some(p) = self.player.as_mut() {
            p.pause();
        }
        cx.notify();
    }

    // ---- cutting: split-and-delete ----

    /// Split the video track at the playhead ('S' key / Split button).
    fn split_click(&mut self, cx: &mut Context<Self>) {
        if self.state.split_at(self.playhead) {
            self.history.commit(self.state.clone());
            self.status = Some(format!("Split at {:.1}s", self.playhead).into());
            cx.notify();
        }
    }

    /// Remove the selected video segment (Delete/Backspace). Shown dimmed,
    /// skipped during playback, excluded at export — never actually deleted
    /// from `segments` so it can be restored.
    fn delete_selected_segment(&mut self, cx: &mut Context<Self>) {
        if let state::Selection::VideoSegment(ix) = self.selection {
            self.state.delete_segment(ix);
            self.history.commit(self.state.clone());
            cx.notify();
        }
    }

    // ---- zoom segments ----

    /// "+ Zoom" toolbar affordance: insert a new zoom segment centered on
    /// the playhead (default duration 3.0s, frame-center framing, the
    /// currently-selected preset level), then select it so the crop rect and
    /// quick inspector appear for the user to drag into place. Replaces the
    /// old click-to-place `zoom_arming` flow.
    pub(super) fn add_zoom_at_playhead(&mut self, cx: &mut Context<Self>) {
        let level = ZOOM_LEVELS[self.zoom_level_ix].1;
        let ix = self.state.add_zoom(
            self.playhead,
            self.info.width as f64 / 2.0,
            self.info.height as f64 / 2.0,
            level,
            3.0,
            self.info.duration_s,
        );
        self.selection = state::Selection::Zoom(ix);
        self.history.commit(self.state.clone());
        self.status = Some(format!("Zoom {level}× added at {:.1}s", self.playhead).into());
        self.refresh_display(cx);
    }

    // ---- captions ----

    fn generate_captions(&mut self, cx: &mut Context<Self>) {
        if self.cc_status.is_some() {
            return;
        }
        self.cc_status = Some("CC: starting…".into());
        self.cc_running = true;
        *self.cc_progress.lock().unwrap() = "starting…".to_string();
        let input = self.path.clone();
        let srt_path = self.path.with_extension("srt");
        let progress = self.cc_progress.clone();

        // Status ticker: mirrors export()'s progress loop, since whisper
        // reports progress via a callback (not an async stream) and the
        // reader here just needs to poll it periodically.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(400)).await;
                let live = this.update(cx, |this: &mut Self, cx| {
                    if this.cc_running {
                        let msg = this.cc_progress.lock().unwrap().clone();
                        this.cc_status = Some(format!("CC: {msg}").into());
                        cx.notify();
                    }
                    this.cc_running
                });
                if !matches!(live, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let progress2 = progress.clone();
            let srt_out = srt_path.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    ashot_core::captions::generate(&input, &srt_out, |s| {
                        *progress2.lock().unwrap() = s.to_string();
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.cc_running = false;
                match result {
                    Ok(()) => {
                        this.srt = Some(srt_path.clone());
                        this.state.burn_cc = true;
                        this.history.commit(this.state.clone());
                        this.cc_status = Some("CC ✓".into());
                        this.reload_cues(cx);
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
        let overlay_png = if self.state.annotations.is_empty() {
            None
        } else {
            let mut overlay = Pixmap::new(self.info.width, self.info.height);
            overlay.as_mut().map(|pm| self.renderer.render(pm, &self.state.annotations));
            overlay.and_then(|pm| {
                let p = std::env::temp_dir().join(format!("ashot-overlay-{}.png", std::process::id()));
                pm.save_png(&p).ok().map(|_| p)
            })
        };

        let spec = video::ExportSpec {
            input: self.path.clone(),
            output,
            keep: self.state.keep_ranges(self.info.duration_s),
            zooms: self.state.zooms.clone(),
            overlay_png,
            srt: if self.state.burn_cc { self.srt.clone() } else { None },
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
            "s" => self.split_click(cx),
            "delete" | "backspace" => self.delete_selected_segment(cx),
            "z" if ev.keystroke.modifiers.control && ev.keystroke.modifiers.shift => {
                if let Some(new) = self.history.redo() {
                    self.state = new.clone();
                    self.clamp_selection();
                    self.refresh_display(cx);
                }
            }
            "z" if ev.keystroke.modifiers.control => {
                if let Some(new) = self.history.undo() {
                    self.state = new.clone();
                    self.clamp_selection();
                    self.refresh_display(cx);
                }
            }
            "escape" if self.selection != state::Selection::None => {
                self.selection = state::Selection::None;
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

    /// Reset `selection` to `None` if it now indexes past the end of the
    /// current `state`'s segments/zooms (after undo/redo, delete, remove).
    fn clamp_selection(&mut self) {
        let out_of_bounds = match self.selection {
            state::Selection::None => false,
            state::Selection::VideoSegment(i) => i >= self.state.segments.len(),
            state::Selection::Zoom(i) => i >= self.state.zooms.len(),
        };
        if out_of_bounds {
            self.selection = state::Selection::None;
        }
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

        // ---- toolbar: tools + transport only (export/CC/zoom-presets live
        // in the inspector now, see `inspector.rs`) ----
        let mut toolbar = div()
            .flex_none()
            .w_full()
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
            .child(self.chip(("split", 0), "✂ Split".into(), false, cx, |this, cx| {
                this.split_click(cx)
            }));
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
        toolbar = toolbar.child(div().flex_1()).child(
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
                            format!("{:.1}s / {:.1}s", self.playhead, self.info.duration_s).into()
                        }),
                ),
        );

        // ---- timeline (ruler + video/zoom/audio/captions lanes) ----
        let timeline_el = timeline::render_timeline(self, window, cx);

        // ---- preview column (flex_1 in the middle row, absolute overlay
        // children positioned relative to its own top-left via `fit()`) ----
        let mut preview_col = div()
            .id("veditor")
            .relative()
            .flex_1()
            .h_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(theme::bg())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key_down(ev, cx)));

        if let Some(frame) = &self.frame {
            // Live zoom as a layout transform: the eased crop window
            // (`crop_rect_for`, same math as export) maps onto the fit rect
            // by scaling the full-frame texture up and offsetting it, clipped
            // by the column's `overflow_hidden`. No per-frame pixmap work.
            let (ix, iy, iw, ih) = match state::crop_rect_for(
                self.playhead,
                &self.state.zooms,
                self.info.width as f64,
                self.info.height as f64,
            ) {
                Some((x0, y0, vw, _vh)) => {
                    let k = self.info.width as f64 / vw;
                    let zs = scale * k as f32;
                    (ox - x0 as f32 * zs, oy - y0 as f32 * zs, dw * k as f32, dh * k as f32)
                }
                None => (ox, oy, dw, dh),
            };
            preview_col = preview_col.child(
                img(frame.clone())
                    .absolute()
                    .left(px(ix))
                    .top(px(iy))
                    .w(px(iw))
                    .h(px(ih)),
            );
            // Annotations layer: native-res transparent image stacked with
            // the exact same transform as the (preview-scaled) video frame.
            if let Some(overlay) = &self.overlay_frame {
                preview_col = preview_col.child(
                    img(overlay.clone())
                        .absolute()
                        .left(px(ix))
                        .top(px(iy))
                        .w(px(iw))
                        .h(px(ih)),
                );
            }
        }
        // Input layer over the preview.
        preview_col = preview_col.child(
            div()
                .id("vcanvas")
                .absolute()
                .left(px(ox))
                .top(px(oy))
                .w(px(dw))
                .h(px(dh))
                .cursor(if self.tool.is_some() {
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
                    preview_col = preview_col.child(d);
                }
                Some(Tool::Arrow) => {
                    let (fx, fy) = to_win(sx, sy);
                    let (tx, ty) = to_win(cx_, cy_);
                    preview_col = preview_col.child(
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

        // Crop-rect overlay on the preview for the selected zoom segment —
        // drag body to recenter, drag a corner to change level (aspect
        // stays locked since crop_for/peak_crop_rect divide both W and H by
        // the same `level` scalar). Its readouts/presets/Remove live in the
        // inspector panel (`inspector.rs`), not here.
        if let state::Selection::Zoom(ix) = self.selection {
            if let Some((x0, y0, vw, vh)) = self.peak_crop_rect(ix) {
                let (wx, wy) = (ox + x0 as f32 * scale, oy + y0 as f32 * scale);
                let (ww, wh) = (vw as f32 * scale, vh as f32 * scale);
                preview_col = preview_col.child(
                    div()
                        .absolute()
                        .left(px(wx))
                        .top(px(wy))
                        .w(px(ww))
                        .h(px(wh))
                        .border_2()
                        .border_color(theme::accent())
                        .cursor(CursorStyle::PointingHand),
                );
            }
        }

        // Live caption burn-in preview — pure text overlay (no pixmap work,
        // same philosophy as the live-zoom layout transform above). Mirrors
        // what `export` bakes into the frames when `burn_cc` is on.
        if self.state.burn_cc {
            if let Some(cue) = self
                .cues
                .iter()
                .find(|c| c.start_s <= self.playhead && self.playhead < c.end_s)
            {
                preview_col = preview_col.child(
                    div()
                        .absolute()
                        .left(px(ox))
                        .top(px(oy + dh - 32.0))
                        .w(px(dw))
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            div()
                                .max_w(px(dw * 0.8))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(gpui::rgba(0x00000099))
                                .text_sm()
                                .text_center()
                                .text_color(gpui::white())
                                .child(cue.text.clone()),
                        ),
                );
            }
        }

        // ---- root: flex column [toolbar / (preview | inspector) / timeline] ----
        let middle_row = div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_row()
            .child(preview_col)
            .child(render_inspector(self, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .child(toolbar)
            .child(middle_row)
            .child(timeline_el)
    }
}
