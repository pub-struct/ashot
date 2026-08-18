//! The `ashot ui` entry point — a floating pill toolbar (user-sketched design):
//!
//!   [Screenshot|Record] │ [Full|Crop] ([720p|1080p|2K] [🎤 mic]) (●)
//!
//! Selecting Crop enters a **freeze-frame** crop session: the pill unmaps, the
//! desktop is captured, and that still is shown fullscreen in an *opaque*
//! window with the pill floating on top. The region can be drawn, moved and
//! resized freely; nothing happens until the red button fires, and what fires
//! is exactly the pixels that were framed.
//!
//! Freeze-frame rather than a transparent live window, deliberately:
//! compositors that blur behind translucent surfaces (Hyprland ships
//! `decoration:blur` on) smear the very desktop the user is trying to frame,
//! and over a live window the screen can change between framing and firing.
//! A still can do neither.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, point, prelude::*, px, App, Application, Bounds, Context, CursorStyle, FocusHandle,
    Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, RenderImage, SharedString, Size, Window, WindowBackgroundAppearance, WindowBounds,
    WindowOptions,
};
use tiny_skia::Pixmap;

use ashot_core::record::{RecordOptions, RESOLUTIONS};

use crate::{img::to_render_image, overlay, recorder, theme};

pub fn run() -> anyhow::Result<()> {
    // Explicit quit mode: launcher flows close their window before opening the
    // next one, and Linux's default (quit on last window closed) would kill
    // the app mid-transition. Every terminal state calls cx.quit() itself.
    Application::with_platform(gpui_platform::current_platform(false))
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(|cx: &mut App| {
            match std::env::var("ASHOT_TEST_MODE").unwrap_or_default().as_str() {
                // Test hook: straight to a full-screen recording (no launcher).
                "record-start" => start_recording_flow(None, Some(1080), None, false, cx),
                // Test hooks: straight into a freeze-frame crop session, so it
                // can be exercised without input injection. `crop-session`
                // and `crop-fire` preset a region (see `LauncherView::new`)
                // and `crop-fire` then fires on it; `crop-empty` shows the
                // not-yet-framed state.
                "crop-session" | "crop-fire" | "crop-empty" | "crop-refresh" => {
                    freeze_then_crop_session(
                        LauncherState { scope: Scope::Crop, ..Default::default() },
                        cx,
                    );
                }
                _ => open_window(cx),
            }
            cx.activate(true);
        });
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Screenshot,
    Record,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Scope {
    Full,
    Crop,
}

/// The pill's selections, carried across window swaps (plain ↔ crop session).
#[derive(Clone)]
pub struct LauncherState {
    pub mode: Mode,
    pub scope: Scope,
    pub res_ix: usize,
    pub mic_ix: usize,
    pub system_audio: bool,
    /// Region to restore when a crop session re-opens over a fresh still
    /// (window coords — every session is fullscreen on the same display, so
    /// they are directly comparable). Only `refresh_frame` sets it; entering
    /// a session from the pill always starts unframed.
    pub sel: Option<(Point<Pixels>, Point<Pixels>)>,
}

impl Default for LauncherState {
    fn default() -> Self {
        // `res_ix == RESOLUTIONS.len()` is the virtual "Native" entry (no
        // downscale) — the default, so a 2K/4K screen records at full detail.
        Self {
            mode: Mode::Screenshot,
            scope: Scope::Full,
            res_ix: RESOLUTIONS.len(),
            mic_ix: 0,
            system_audio: false,
            sel: None,
        }
    }
}

pub fn open_window(cx: &mut App) {
    open_window_with(LauncherState::default(), cx);
}

/// The plain floating pill (small transparent window).
pub fn open_window_with(state: LauncherState, cx: &mut App) {
    let bounds = Bounds::centered(None, Size { width: px(820.), height: px(280.) }, cx);
    open_launcher(state, None, WindowBounds::Windowed(bounds), cx);
}

/// The freeze-frame crop session: fullscreen opaque window over `frame`, pill
/// on top.
pub fn open_crop_session(state: LauncherState, frame: Pixmap, cx: &mut App) {
    let display = cx
        .primary_display()
        .map(|d| d.bounds())
        .unwrap_or_else(|| Bounds::from_corners(Point::default(), Point::new(px(1920.), px(1080.))));
    open_launcher(state, Some(Frozen::new(frame)), WindowBounds::Fullscreen(display), cx);
}

fn open_launcher(
    state: LauncherState,
    frozen: Option<Frozen>,
    bounds: WindowBounds,
    cx: &mut App,
) {
    let crop_session = frozen.is_some();
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            titlebar: None,
            // The bare pill needs a translucent window to get its shape. The
            // crop session must not: a translucent fullscreen surface makes
            // blur-behind compositors blur the whole desktop underneath it,
            // which is exactly the thing being framed.
            window_background: if crop_session {
                WindowBackgroundAppearance::Opaque
            } else {
                WindowBackgroundAppearance::Transparent
            },
            is_resizable: false,
            is_movable: !crop_session,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| LauncherView::new(state.clone(), frozen.clone(), window, cx));
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
        eprintln!("failed to open launcher window");
        cx.quit();
    }
}

/// The still a crop session frames: the pixmap (source of the crop) and its
/// GPU-side copy (what is on screen).
#[derive(Clone)]
struct Frozen {
    pixmap: Arc<Pixmap>,
    image: Arc<RenderImage>,
}

impl Frozen {
    fn new(pixmap: Pixmap) -> Self {
        let image = to_render_image(&pixmap);
        Self { pixmap: Arc::new(pixmap), image }
    }
}

/// What the pointer is over, which decides both the cursor and what a press
/// there would start.
#[derive(Clone, Copy, PartialEq)]
enum Grab {
    New,
    Move,
    Resize { left: bool, right: bool, top: bool, bottom: bool },
}

/// What a press-and-drag on the crop surface is currently doing.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// Drawing a fresh region; `sel_start` is the anchor.
    New,
    /// Sliding the whole region: the grab point and the corners it had then.
    Move { grab: Point<Pixels>, orig: (Point<Pixels>, Point<Pixels>) },
    /// Dragging an edge or corner: `sel_start` is pinned to the opposite
    /// corner and `sel_end` follows the pointer on the unlocked axes.
    Resize { free_x: bool, free_y: bool },
}

struct LauncherView {
    mode: Mode,
    scope: Scope,
    res_ix: usize,
    /// (device, label): None device = don't record mic.
    mics: Vec<(Option<String>, SharedString)>,
    mic_ix: usize,
    mic_open: bool,
    system_audio: bool,
    /// Fullscreen freeze-frame selection mode; `frozen` holds the still.
    crop_session: bool,
    frozen: Option<Frozen>,
    sel_start: Option<Point<Pixels>>,
    sel_end: Option<Point<Pixels>>,
    drag: Drag,
    /// Last pointer position, so the cursor can advertise the grab under it.
    hover: Option<Point<Pixels>>,
    /// Pre-record mic check: running pipeline (dropping it stops the child),
    /// whether the user hears themselves, whether the recording DSP chain is
    /// applied, latest (rms, peak) dBFS, and a generation counter so a
    /// restart retires the previous poll loop.
    mic_test: Option<ashot_core::micmon::MicMonitor>,
    mic_hear: bool,
    mic_dsp_on: bool,
    mic_level: (f32, f32),
    mic_test_gen: usize,
    focus_handle: FocusHandle,
}

impl LauncherView {
    fn new(
        state: LauncherState,
        frozen: Option<Frozen>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut mics: Vec<(Option<String>, SharedString)> = vec![
            (None, "Don't record mic".into()),
            (Some("default".into()), "Default microphone".into()),
        ];
        for (name, desc) in ashot_core::record::list_microphones() {
            mics.push((Some(name), truncate(&desc, 42).into()));
        }

        let test_mode = std::env::var("ASHOT_TEST_MODE").unwrap_or_default();
        let mode = if test_mode.starts_with("record") { Mode::Record } else { state.mode };
        // Test hooks that drive the pill down a real path (window close →
        // async flow) after a beat, since input can't be injected. The
        // re-shoot one runs once: the session it re-opens arrives with a
        // carried region, which is the signal not to schedule another.
        let auto: Option<fn(&mut Self, &mut Window, &mut Context<Self>)> =
            match test_mode.as_str() {
                "auto-full" | "crop-fire" => Some(Self::fire),
                "crop-refresh" if state.sel.is_none() => Some(Self::refresh_frame),
                _ => None,
            };
        if let Some(action) = auto {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                this.update_in(cx, action).ok();
            })
            .detach();
        }
        // Test hook: preset a region so the crop chrome renders without input.
        let preset = (matches!(test_mode.as_str(), "crop-session" | "crop-fire" | "crop-refresh")
            && frozen.is_some())
            .then(|| (point(px(320.), px(220.)), point(px(1180.), px(760.))));
        // …otherwise a region survives a re-shoot (R) and nothing else.
        let sel = preset.or(state.sel.filter(|_| frozen.is_some()));
        let mic_open = test_mode == "record-mics";
        // Worst-case layout check: hook selects the longest mic name.
        let mic_ix = if mic_open {
            mics.len().saturating_sub(2).max(0)
        } else {
            state.mic_ix.min(mics.len() - 1)
        };

        Self {
            mode,
            scope: state.scope,
            res_ix: state.res_ix,
            mics,
            mic_ix,
            mic_open,
            system_audio: state.system_audio,
            crop_session: frozen.is_some(),
            frozen,
            sel_start: sel.map(|(a, _)| a),
            sel_end: sel.map(|(_, b)| b),
            drag: Drag::None,
            hover: None,
            mic_test: None,
            mic_hear: false,
            mic_dsp_on: true,
            mic_level: (ashot_core::micmon::FLOOR_DB, ashot_core::micmon::FLOOR_DB),
            mic_test_gen: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    // ---- mic check ----

    fn start_mic_test(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.mics[self.mic_ix].0.clone() else { return };
        match ashot_core::micmon::start(Some(&device), self.mic_dsp_on, self.mic_hear) {
            Ok(m) => {
                self.mic_test = Some(m);
                self.mic_level = (ashot_core::micmon::FLOOR_DB, ashot_core::micmon::FLOOR_DB);
                self.mic_test_gen += 1;
                let gen = self.mic_test_gen;
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_millis(80)).await;
                        let alive = this.update(cx, |this, cx| {
                            if this.mic_test_gen != gen {
                                return false;
                            }
                            match &this.mic_test {
                                Some(m) => {
                                    this.mic_level = m.level();
                                    cx.notify();
                                    true
                                }
                                None => false,
                            }
                        });
                        if !matches!(alive, Ok(true)) {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => eprintln!("mic check failed to start: {e}"),
        }
    }

    fn stop_mic_test(&mut self) {
        // Dropping the monitor kills the gst child.
        self.mic_test = None;
    }

    fn restart_mic_test(&mut self, cx: &mut Context<Self>) {
        if self.mic_test.is_some() {
            self.stop_mic_test();
            self.start_mic_test(cx);
        }
    }

    fn snapshot(&self) -> LauncherState {
        LauncherState {
            mode: self.mode,
            scope: self.scope,
            res_ix: self.res_ix,
            mic_ix: self.mic_ix,
            system_audio: self.system_audio,
            sel: None,
        }
    }

    /// Pointer slop around a selection edge that counts as grabbing it.
    const GRIP: f32 = 9.0;

    /// Selection rect in window coords (min 3px each way).
    fn sel_rect(&self) -> Option<(Pixels, Pixels, Pixels, Pixels)> {
        let (a, b) = (self.sel_start?, self.sel_end?);
        let x0 = a.x.min(b.x);
        let y0 = a.y.min(b.y);
        let x1 = a.x.max(b.x);
        let y1 = a.y.max(b.y);
        if f32::from(x1 - x0) < 3.0 || f32::from(y1 - y0) < 3.0 {
            return None;
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    /// Selection mapped to frame pixels — the crop for both the screenshot
    /// (cut straight out of the still) and the recorder's `videocrop`, since
    /// the portal screenshot and the screencast stream are the same monitor
    /// at its native mode.
    ///
    /// Read off the frame we already hold. The previous version asked Wayland
    /// for the monitor list on every call, and `crop_label` calls it once per
    /// frame — a fresh connection plus three roundtrips per mouse-move, which
    /// is what made dragging a region feel like it was fighting back.
    fn sel_to_pixels(&self, viewport: Size<Pixels>) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = self.sel_rect()?;
        let frame = self.frozen.as_ref()?;
        let (fw, fh) = (frame.pixmap.width(), frame.pixmap.height());
        let sx = fw as f32 / f32::from(viewport.width).max(1.0);
        let sy = fh as f32 / f32::from(viewport.height).max(1.0);
        let ix = ((f32::from(x) * sx).round().max(0.0) as u32).min(fw.saturating_sub(1));
        let iy = ((f32::from(y) * sy).round().max(0.0) as u32).min(fh.saturating_sub(1));
        let iw = ((f32::from(w) * sx).round().max(1.0) as u32).min(fw - ix);
        let ih = ((f32::from(h) * sy).round().max(1.0) as u32).min(fh - iy);
        Some((ix, iy, iw, ih))
    }

    fn is_dragging(&self) -> bool {
        !matches!(self.drag, Drag::None)
    }

    /// What pressing at `p` would grab. Edges win over the interior so a thin
    /// selection stays resizable.
    fn grab_at(&self, p: Point<Pixels>) -> Grab {
        let Some((x, y, w, h)) = self.sel_rect() else { return Grab::New };
        let g = Self::GRIP;
        let (pxx, pyy) = (f32::from(p.x), f32::from(p.y));
        let (x0, y0) = (f32::from(x), f32::from(y));
        let (x1, y1) = (x0 + f32::from(w), y0 + f32::from(h));
        if pxx < x0 - g || pxx > x1 + g || pyy < y0 - g || pyy > y1 + g {
            return Grab::New;
        }
        let (near_l, near_r) = ((pxx - x0).abs() <= g, (pxx - x1).abs() <= g);
        let (near_t, near_b) = ((pyy - y0).abs() <= g, (pyy - y1).abs() <= g);
        if near_l || near_r || near_t || near_b {
            Grab::Resize {
                left: near_l && !near_r,
                right: near_r,
                top: near_t && !near_b,
                bottom: near_b,
            }
        } else {
            Grab::Move
        }
    }

    /// Cursor for the current drag, or for whatever sits under the pointer.
    fn crop_cursor(&self) -> CursorStyle {
        match self.drag {
            Drag::Move { .. } => CursorStyle::ClosedHand,
            Drag::New | Drag::Resize { .. } => CursorStyle::Crosshair,
            Drag::None => match self.hover.map(|p| self.grab_at(p)) {
                Some(Grab::Move) => CursorStyle::OpenHand,
                Some(Grab::Resize { left, right, top, bottom }) => {
                    match (left || right, top || bottom) {
                        (true, true) if left == top => CursorStyle::ResizeUpLeftDownRight,
                        (true, true) => CursorStyle::ResizeUpRightDownLeft,
                        (true, false) => CursorStyle::ResizeLeftRight,
                        (false, true) => CursorStyle::ResizeUpDown,
                        (false, false) => CursorStyle::Crosshair,
                    }
                }
                _ => CursorStyle::Crosshair,
            },
        }
    }

    fn drag_start(&mut self, p: Point<Pixels>) {
        match self.grab_at(p) {
            Grab::New => {
                self.sel_start = Some(p);
                self.sel_end = Some(p);
                self.drag = Drag::New;
            }
            Grab::Move => {
                let (Some(a), Some(b)) = (self.sel_start, self.sel_end) else { return };
                self.drag = Drag::Move { grab: p, orig: (a, b) };
            }
            Grab::Resize { left, right, top, bottom } => {
                let Some((x, y, w, h)) = self.sel_rect() else { return };
                let (x0, y0, x1, y1) = (x, y, x + w, y + h);
                // Pin the corner opposite the grabbed edge; the other corner
                // tracks the pointer, but only on the axes that were grabbed.
                let (fx, mx, free_x) = if left {
                    (x1, x0, true)
                } else if right {
                    (x0, x1, true)
                } else {
                    (x0, x1, false)
                };
                let (fy, my, free_y) = if top {
                    (y1, y0, true)
                } else if bottom {
                    (y0, y1, true)
                } else {
                    (y0, y1, false)
                };
                self.sel_start = Some(point(fx, fy));
                self.sel_end = Some(point(mx, my));
                self.drag = Drag::Resize { free_x, free_y };
            }
        }
    }

    fn drag_to(&mut self, p: Point<Pixels>, viewport: Size<Pixels>) {
        match self.drag {
            Drag::None => {}
            Drag::New => self.sel_end = Some(p),
            Drag::Resize { free_x, free_y } => {
                let Some(e) = self.sel_end else { return };
                let x = if free_x { p.x } else { e.x };
                let y = if free_y { p.y } else { e.y };
                self.sel_end = Some(point(x, y));
            }
            Drag::Move { grab, orig: (a, b) } => {
                // Clamp the shift rather than the corners, so dragging a
                // region against a screen edge slides it instead of squashing.
                let lo_x = -a.x.min(b.x);
                let hi_x = (viewport.width - a.x.max(b.x)).max(lo_x);
                let lo_y = -a.y.min(b.y);
                let hi_y = (viewport.height - a.y.max(b.y)).max(lo_y);
                let dx = (p.x - grab.x).clamp(lo_x, hi_x);
                let dy = (p.y - grab.y).clamp(lo_y, hi_y);
                self.sel_start = Some(point(a.x + dx, a.y + dy));
                self.sel_end = Some(point(b.x + dx, b.y + dy));
            }
        }
    }

    /// Crop segment: enter (or restart) the live crop session.
    fn enter_crop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.crop_session {
            // Already in session: clicking Crop clears the selection to redraw.
            self.sel_start = None;
            self.sel_end = None;
            cx.notify();
            return;
        }
        let mut state = self.snapshot();
        state.scope = Scope::Crop;
        // The session may sit open for a while; don't hold the mic through it.
        self.stop_mic_test();
        window.remove_window();
        freeze_then_crop_session(state, cx);
    }

    /// Re-take the still under the session, keeping the framed region — for
    /// when the desktop has moved on since the session opened. The window has
    /// to go for the shutter (it would otherwise photograph itself), so this
    /// is a close-capture-reopen with the selection carried across.
    fn refresh_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut state = self.snapshot();
        state.scope = Scope::Crop;
        state.sel = self.sel_start.zip(self.sel_end);
        self.stop_mic_test();
        window.remove_window();
        freeze_then_crop_session(state, cx);
    }

    /// Leave the crop session back to the plain pill.
    fn leave_crop(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let mut state = self.snapshot();
        state.scope = scope;
        window.remove_window();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| open_window_with(state, cx));
        })
        .detach();
    }

    /// The red button. In a crop session this needs a selection; execution
    /// closes our window first so it is never part of the result.
    fn fire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let height = RESOLUTIONS.get(self.res_ix).map(|(_, h, _)| *h);
        let mic = self.mics[self.mic_ix].0.clone();
        let crop = if self.crop_session {
            match self.sel_to_pixels(window.viewport_size()) {
                Some(c) => Some(c),
                None => return, // nothing framed yet — draw a region first
            }
        } else if self.scope == Scope::Crop {
            // Crop armed without a session (edge case) — enter one instead.
            self.enter_crop(window, cx);
            return;
        } else {
            None
        };
        // Release the mic before the recording pipeline opens it.
        self.stop_mic_test();
        match (self.mode, crop) {
            (Mode::Screenshot, Some(rect)) => {
                // The framed screenshot is already taken — the still under the
                // selection *is* the capture. Cut it here instead of tearing
                // down and asking the portal again: no second round-trip, and
                // no window for the screen to change out from under the frame.
                let frame = self.frozen.as_ref().expect("a crop session holds a still");
                overlay::open_window(
                    (*frame.pixmap).clone(),
                    overlay::OverlayStart::PreviewRegion(rect),
                    cx,
                );
                // Hand over before unmapping, so the desktop never flashes
                // between the two fullscreen windows.
                window.remove_window();
            }
            (Mode::Screenshot, None) => {
                window.remove_window();
                capture_then_overlay(overlay::OverlayStart::PreviewFull, cx)
            }
            (Mode::Record, crop) => {
                window.remove_window();
                start_recording_flow(crop, height, mic, self.system_audio, cx)
            }
        }
    }

    fn segment(
        &self,
        id: (&'static str, usize),
        label: SharedString,
        active: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_3()
            .py_1()
            .flex_none()
            .flex()
            .justify_center()
            .rounded_full()
            .cursor(CursorStyle::PointingHand)
            .text_sm()
            .when(active, |d| d.bg(theme::accent()).text_color(gpui::rgb(0xffffff)))
            .when(!active, |d| {
                d.text_color(theme::text_muted()).hover(|s| s.bg(theme::surface_hover()))
            })
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    handler(this, window, cx)
                }),
            )
    }

    fn seg_group(&self) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_0p5()
            .p_0p5()
            .rounded_full()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
    }

    fn mic_label(&self) -> SharedString {
        match self.mic_ix {
            0 => "🎤 Off".into(),
            1 => "🎤 Default".into(),
            _ => format!("🎤 {}", truncate(&self.mics[self.mic_ix].1, 12)).into(),
        }
    }

    fn crop_label(&self, viewport: Size<Pixels>) -> SharedString {
        if self.crop_session {
            match self.sel_to_pixels(viewport) {
                Some((_, _, w, h)) => format!("Crop {w}×{h}").into(),
                None => "Crop…".into(),
            }
        } else {
            "Crop".into()
        }
    }

    /// The pill element (shared by the plain window and the crop session).
    fn pill(&self, viewport: Size<Pixels>, cx: &mut Context<Self>) -> gpui::Div {
        let record = self.mode == Mode::Record;

        let mut pill = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_full()
            .bg(theme::bg())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .child(
                self.seg_group()
                    .child(self.segment(
                        ("mode", 0),
                        "Screenshot".into(),
                        !record,
                        cx,
                        |this, _, cx| {
                            this.mode = Mode::Screenshot;
                            this.mic_open = false;
                            this.stop_mic_test();
                            cx.notify();
                        },
                    ))
                    .child(self.segment(("mode", 1), "⏺ Record".into(), record, cx, |this, _, cx| {
                        this.mode = Mode::Record;
                        cx.notify();
                    })),
            )
            .child(div().w(px(1.)).h(px(24.)).bg(theme::border()))
            .child(
                self.seg_group()
                    .child(self.segment(
                        ("scope", 0),
                        "Full".into(),
                        self.scope == Scope::Full && !self.crop_session,
                        cx,
                        |this, window, cx| {
                            if this.crop_session {
                                this.leave_crop(Scope::Full, window, cx);
                            } else {
                                this.scope = Scope::Full;
                                cx.notify();
                            }
                        },
                    ))
                    .child(self.segment(
                        ("scope", 1),
                        self.crop_label(viewport),
                        self.scope == Scope::Crop || self.crop_session,
                        cx,
                        |this, window, cx| this.enter_crop(window, cx),
                    )),
            );

        if record {
            let mut res_group = self.seg_group();
            for ix in 0..=RESOLUTIONS.len() {
                let label = match RESOLUTIONS.get(ix) {
                    Some((label, ..)) => {
                        if ix == 2 {
                            "2K"
                        } else {
                            label
                        }
                    }
                    None => "Native",
                };
                res_group = res_group.child(self.segment(
                    ("res", ix),
                    label.into(),
                    self.res_ix == ix,
                    cx,
                    move |this, _, cx| {
                        this.res_ix = ix;
                        cx.notify();
                    },
                ));
            }
            pill = pill.child(res_group).child(
                div()
                    .id("mic")
                    .px_3()
                    .py_1()
                    .flex_none()
                    .rounded_full()
                    .cursor(CursorStyle::PointingHand)
                    .bg(theme::surface())
                    .border_1()
                    .border_color(if self.mic_ix > 0 { theme::accent() } else { theme::border() })
                    .text_sm()
                    .text_color(if self.mic_ix > 0 { theme::text() } else { theme::text_muted() })
                    .child(self.mic_label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.mic_open = !this.mic_open;
                            cx.notify();
                        }),
                    ),
            );
            let sys = self.system_audio;
            pill = pill.child(
                div()
                    .id("sysaudio")
                    .px_3()
                    .py_1()
                    .flex_none()
                    .rounded_full()
                    .cursor(CursorStyle::PointingHand)
                    .bg(theme::surface())
                    .border_1()
                    .border_color(if sys { theme::accent() } else { theme::border() })
                    .text_sm()
                    .text_color(if sys { theme::text() } else { theme::text_muted() })
                    .child(if sys { "🔊 System" } else { "🔇 System" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.system_audio = !this.system_audio;
                            cx.notify();
                        }),
                    ),
            );
            if self.mic_ix > 0 {
                let testing = self.mic_test.is_some();
                pill = pill.child(
                    div()
                        .id("mictest")
                        .px_3()
                        .py_1()
                        .flex_none()
                        .rounded_full()
                        .cursor(CursorStyle::PointingHand)
                        .bg(theme::surface())
                        .border_1()
                        .border_color(if testing { theme::accent() } else { theme::border() })
                        .text_sm()
                        .text_color(if testing { theme::text() } else { theme::text_muted() })
                        .child("🎧 Test")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                if this.mic_test.is_some() {
                                    this.stop_mic_test();
                                } else {
                                    this.start_mic_test(cx);
                                }
                                cx.notify();
                            }),
                        ),
                );
            }
        }

        // In a crop session the button does nothing until a region exists —
        // say so, rather than swallowing the click and looking broken.
        let armed = !self.crop_session || self.sel_rect().is_some();
        pill.child(div().flex_1()).child(
            div()
                .id("fire")
                .w(px(40.))
                .h(px(40.))
                .flex_none()
                .rounded_full()
                .when(armed, |d| {
                    d.cursor(CursorStyle::PointingHand)
                        .bg(gpui::rgb(0xff3b30))
                        .border_color(gpui::rgb(0xffffff))
                        .hover(|s| s.bg(gpui::rgb(0xff5b4d)))
                })
                .when(!armed, |d| {
                    d.cursor(CursorStyle::OperationNotAllowed)
                        .bg(gpui::rgba(0xff3b3055))
                        .border_color(gpui::rgba(0xffffff55))
                })
                .border_2()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| this.fire(window, cx)),
                ),
        )
    }

    /// Meter + toggles shown below the pill while the mic check runs. The
    /// fill bar is RMS, the tick is peak; calibrate the external mic's gain
    /// until peaks sit in the yellow (−12…−6 dB).
    fn mic_check_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const METER_W: f32 = 240.0;
        let (rms, peak) = self.mic_level;
        let frac = |db: f32| {
            ((db - ashot_core::micmon::FLOOR_DB) / -ashot_core::micmon::FLOOR_DB).clamp(0.0, 1.0)
        };
        let fill_color = if peak > -6.0 {
            gpui::rgb(0xff3b30)
        } else if peak > -12.0 {
            gpui::rgb(0xffcc00)
        } else {
            gpui::rgb(0x34c759)
        };
        let toggle = |id: &'static str,
                      label: SharedString,
                      on: bool,
                      cx: &mut Context<Self>,
                      handler: fn(&mut Self, &mut Context<Self>)| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .flex_none()
                .rounded_full()
                .cursor(CursorStyle::PointingHand)
                .bg(theme::surface())
                .border_1()
                .border_color(if on { theme::accent() } else { theme::border() })
                .text_sm()
                .text_color(if on { theme::text() } else { theme::text_muted() })
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        handler(this, cx);
                        cx.notify();
                    }),
                )
        };
        div()
            .mt_2()
            .w(px(440.))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_lg()
            .bg(theme::bg())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .relative()
                            .w(px(METER_W))
                            .h(px(10.))
                            .rounded_full()
                            .bg(theme::surface())
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .w(px(frac(rms) * METER_W))
                                    .h_full()
                                    .rounded_full()
                                    .bg(fill_color),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left(px((frac(peak) * METER_W - 1.0).max(0.0)))
                                    .top_0()
                                    .w(px(2.))
                                    .h_full()
                                    .bg(gpui::rgb(0xffffff)),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text())
                            .child(format!("{:.0} dB", peak.max(ashot_core::micmon::FLOOR_DB))),
                    )
                    .child(div().flex_1())
                    .child(toggle("mic-hear", "🎧 Hear myself".into(), self.mic_hear, cx, |this, cx| {
                        this.mic_hear = !this.mic_hear;
                        this.restart_mic_test(cx);
                    }))
                    .child(toggle("mic-dsp", "✨ Processing".into(), self.mic_dsp_on, cx, |this, cx| {
                        this.mic_dsp_on = !this.mic_dsp_on;
                        this.restart_mic_test(cx);
                    })),
            )
            .child(
                div().text_xs().text_color(theme::text_muted()).child(
                    "Speak normally; set your mic's gain so peaks sit around −12 to −6 dB. \
                     Use headphones with Hear myself to avoid feedback.",
                ),
            )
    }

    fn mic_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .mt_2()
            .w(px(360.))
            .flex()
            .flex_col()
            .p_1()
            .rounded_lg()
            .bg(theme::bg())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .occlude()
            .children((0..self.mics.len()).map(|ix| {
                let (_, label) = &self.mics[ix];
                let active = self.mic_ix == ix;
                div()
                    .id(("mic-opt", ix))
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .cursor(CursorStyle::PointingHand)
                    .text_sm()
                    .text_color(if active { gpui::rgb(0xffffff) } else { theme::text() })
                    .when(active, |d| d.bg(theme::accent()))
                    .when(!active, |d| d.hover(|s| s.bg(theme::surface_hover())))
                    .child(label.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            this.mic_ix = ix;
                            this.mic_open = false;
                            if ix == 0 {
                                this.stop_mic_test();
                            } else {
                                // Re-meter the newly selected device.
                                this.restart_mic_test(cx);
                            }
                            cx.notify();
                        }),
                    )
            }))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

impl Focusable for LauncherView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LauncherView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let record = self.mode == Mode::Record;
        // The pill blocks drag events from reaching the selection layer.
        let pill = self.pill(viewport, cx).occlude();

        let mut root = div()
            .id("launcher")
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" if this.crop_session => this.leave_crop(Scope::Full, window, cx),
                    // Bare R only: Ctrl-R and friends are not a re-shoot.
                    "r" if this.crop_session && !ev.keystroke.modifiers.modified() => {
                        this.refresh_frame(window, cx)
                    }
                    "escape" => {
                        // Kill the mic-check child before the process exits.
                        this.stop_mic_test();
                        cx.quit()
                    }
                    "enter" => this.fire(window, cx),
                    _ => {}
                }
            }));

        if self.crop_session {
            // The still, 1:1 with the window, under everything else. Opaque —
            // no translucent surface for the compositor to blur behind.
            if let Some(frame) = &self.frozen {
                root = root.child(
                    img(frame.image.clone())
                        .absolute()
                        .left_0()
                        .top_0()
                        .w(viewport.width)
                        .h(viewport.height),
                );
            }
            root = root
                .bg(theme::bg())
                .cursor(self.crop_cursor())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.drag_start(ev.position);
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                    let before = this.crop_cursor();
                    this.hover = Some(ev.position);
                    if this.is_dragging() {
                        this.drag_to(ev.position, window.viewport_size());
                        cx.notify();
                    } else if this.crop_cursor() != before {
                        // Idle moves only repaint when the grab under the
                        // pointer changes, i.e. when the cursor has to change.
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                        if this.is_dragging() {
                            this.drag_to(ev.position, window.viewport_size());
                            this.drag = Drag::None;
                            cx.notify();
                        }
                    }),
                );

            // Scrim over the still; the selection is a hole in it (four slabs).
            match self.sel_rect() {
                None => {
                    root = root.child(div().absolute().inset_0().bg(theme::scrim()));
                }
                Some((x, y, w, h)) => {
                    let right_x = x + w;
                    let bottom_y = y + h;
                    root = root
                        .child(
                            div().absolute().left_0().top_0().w(viewport.width).h(y).bg(theme::scrim()),
                        )
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
                                .border_color(theme::accent()),
                        );

                    // Corner + edge grips: the region stays adjustable instead
                    // of having to be redrawn from scratch.
                    let (mid_x, mid_y) = (x + w / 2.0, y + h / 2.0);
                    for (gx, gy) in [
                        (x, y),
                        (mid_x, y),
                        (right_x, y),
                        (x, mid_y),
                        (right_x, mid_y),
                        (x, bottom_y),
                        (mid_x, bottom_y),
                        (right_x, bottom_y),
                    ] {
                        root = root.child(
                            div()
                                .absolute()
                                .left(gx - px(4.))
                                .top(gy - px(4.))
                                .w(px(8.))
                                .h(px(8.))
                                .rounded_sm()
                                .bg(gpui::rgb(0xffffff))
                                .border_1()
                                .border_color(theme::accent()),
                        );
                    }

                    // Size chip at the selection, where the eye already is.
                    if let Some((_, _, pw, ph)) = self.sel_to_pixels(viewport) {
                        root = root.child(
                            div()
                                .absolute()
                                .left(x)
                                .top(if bottom_y + px(34.) < viewport.height {
                                    bottom_y + px(10.)
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
                                .child(format!("{pw} × {ph}")),
                        );
                    }
                }
            }

            // The pill floats at the top, and drops to the bottom when the
            // selection reaches up there — it must never cover the frame.
            let pill_top = self.sel_rect().is_none_or(|(_, y, _, _)| y > px(150.));
            let mut stack = div()
                .absolute()
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .child(pill);
            if self.mic_open && record {
                stack = stack.child(self.mic_dropdown(cx));
            }
            if record && self.mic_test.is_some() {
                stack = stack.child(self.mic_check_panel(cx));
            }
            // Kept on screen even once framed: R (re-shoot the still) is most
            // wanted exactly then, and the chip is small enough to live with.
            stack = stack.child(
                div()
                    .mt_2()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(if self.sel_rect().is_some() {
                        "Drag inside to move, edges to resize · ● captures · R re-shoot · Esc back"
                    } else {
                        "Drag to frame the area · R re-shoot the screen · Esc back"
                    }),
            );
            root = root.child(if pill_top { stack.top(px(24.)) } else { stack.bottom(px(24.)) });
        } else {
            root = root.child(div().mt_2().child(pill));
            if self.mic_open && record {
                root = root.child(self.mic_dropdown(cx));
            }
            if record && self.mic_test.is_some() {
                root = root.child(self.mic_check_panel(cx));
            }
        }

        root
    }
}

/// Close-pill → wait for unmap → capture (off the UI thread) → crop session
/// over the resulting still. The wait matters: the pill must be gone before
/// the shutter, or it is baked into the frame the session then crops.
pub fn freeze_then_crop_session(state: LauncherState, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(250)).await;
        let captured = cx
            .background_executor()
            .spawn(async { ashot_core::capture::capture_full() })
            .await;
        match captured {
            Ok(c) => cx.update(|cx| open_crop_session(state, c.pixmap, cx)),
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } })
                );
                cx.update(|cx| cx.quit())
            }
        }
    })
    .detach();
}

/// Close-launcher → wait for unmap → capture (off the UI thread) → overlay.
pub fn capture_then_overlay(start: overlay::OverlayStart, cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(400)).await;
        let captured = cx
            .background_executor()
            .spawn(async { ashot_core::capture::capture_full() })
            .await;
        match captured {
            Ok(c) => cx.update(|cx| overlay::open_window(c.pixmap, start, cx)),
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } })
                );
                cx.update(|cx| cx.quit())
            }
        }
    })
    .detach();
}

/// Start the portal + GPU pipeline on the background executor, then open the
/// recorder status window.
pub fn start_recording_flow(
    crop: Option<(u32, u32, u32, u32)>,
    height: Option<u32>,
    mic: Option<String>,
    system_audio: bool,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        // Let our windows unmap so they aren't in the recording's first frames.
        cx.background_executor().timer(Duration::from_millis(400)).await;
        let started = cx
            .background_executor()
            .spawn(async move {
                ashot_core::record::start_recording(RecordOptions {
                    output: None,
                    height,
                    crop,
                    mic,
                    system_audio,
                    voice_process: true,
                })
            })
            .await;
        match started {
            Ok(recording) => cx.update(|cx| recorder::start(recording, cx)),
            Err(e) => {
                eprintln!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } })
                );
                cx.update(|cx| cx.quit())
            }
        }
    })
    .detach();
}
