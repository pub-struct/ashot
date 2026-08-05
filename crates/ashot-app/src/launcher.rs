//! The `ashot ui` entry point — a floating pill toolbar (user-sketched design):
//!
//!   [Screenshot|Record] │ [Full|Crop] ([720p|1080p|2K] [🎤 mic]) (●)
//!
//! Selecting Crop enters a live crop session: a fullscreen *transparent*
//! window with a light scrim — the real desktop stays visible underneath —
//! where the region can be dragged (and re-dragged) while the pill floats on
//! top. Nothing is captured until the red button fires; at that point our
//! window unmaps first, so the UI is never in the result.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Application, Bounds, Context, CursorStyle, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    SharedString, Size, Window, WindowBackgroundAppearance, WindowBounds, WindowOptions,
};

use ashot_core::record::{RecordOptions, RESOLUTIONS};

use crate::{overlay, recorder, theme};

pub fn run() -> anyhow::Result<()> {
    // Explicit quit mode: launcher flows close their window before opening the
    // next one, and Linux's default (quit on last window closed) would kill
    // the app mid-transition. Every terminal state calls cx.quit() itself.
    Application::with_platform(gpui_platform::current_platform(false))
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(|cx: &mut App| {
            // Test hook: go straight to a full-screen recording (no launcher).
            if std::env::var_os("ASHOT_TEST_MODE").is_some_and(|v| v == "record-start") {
                start_recording_flow(None, Some(1080), None, false, cx);
            } else {
                open_window(cx);
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
}

impl Default for LauncherState {
    fn default() -> Self {
        Self { mode: Mode::Screenshot, scope: Scope::Full, res_ix: 1, mic_ix: 0, system_audio: false }
    }
}

pub fn open_window(cx: &mut App) {
    open_window_with(LauncherState::default(), cx);
}

/// The plain floating pill (small transparent window).
pub fn open_window_with(state: LauncherState, cx: &mut App) {
    let bounds = Bounds::centered(None, Size { width: px(820.), height: px(280.) }, cx);
    open_launcher(state, false, WindowBounds::Windowed(bounds), cx);
}

/// The live crop session (fullscreen transparent window, pill on top).
pub fn open_crop_session(state: LauncherState, cx: &mut App) {
    let display = cx
        .primary_display()
        .map(|d| d.bounds())
        .unwrap_or_else(|| Bounds::from_corners(Point::default(), Point::new(px(1920.), px(1080.))));
    open_launcher(state, true, WindowBounds::Fullscreen(display), cx);
}

fn open_launcher(state: LauncherState, crop_session: bool, bounds: WindowBounds, cx: &mut App) {
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            titlebar: None,
            window_background: WindowBackgroundAppearance::Transparent,
            is_resizable: false,
            is_movable: !crop_session,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| LauncherView::new(state.clone(), crop_session, window, cx));
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

struct LauncherView {
    mode: Mode,
    scope: Scope,
    res_ix: usize,
    /// (device, label): None device = don't record mic.
    mics: Vec<(Option<String>, SharedString)>,
    mic_ix: usize,
    mic_open: bool,
    system_audio: bool,
    /// Fullscreen live-selection mode.
    crop_session: bool,
    sel_start: Option<Point<Pixels>>,
    sel_end: Option<Point<Pixels>>,
    dragging: bool,
    focus_handle: FocusHandle,
}

impl LauncherView {
    fn new(
        state: LauncherState,
        crop_session: bool,
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
        if test_mode == "auto-full" {
            // Exercise the real click path (window close → async flow).
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                this.update_in(cx, |this, window, cx| this.fire(window, cx)).ok();
            })
            .detach();
        }
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
            crop_session,
            sel_start: None,
            sel_end: None,
            dragging: false,
            focus_handle: cx.focus_handle(),
        }
    }

    fn snapshot(&self) -> LauncherState {
        LauncherState {
            mode: self.mode,
            scope: self.scope,
            res_ix: self.res_ix,
            mic_ix: self.mic_ix,
            system_audio: self.system_audio,
        }
    }

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

    /// Selection mapped to stream/image pixels via the monitor's pixel size.
    fn sel_to_stream(&self, viewport: Size<Pixels>) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = self.sel_rect()?;
        let (mon_w, mon_h) = ashot_core::capture::monitors()
            .ok()
            .and_then(|m| m.first().map(|m| (m.pixel_w as f32, m.pixel_h as f32)))
            .unwrap_or((f32::from(viewport.width), f32::from(viewport.height)));
        let sx = mon_w / f32::from(viewport.width).max(1.0);
        let sy = mon_h / f32::from(viewport.height).max(1.0);
        Some((
            (f32::from(x) * sx).round().max(0.0) as u32,
            (f32::from(y) * sy).round().max(0.0) as u32,
            ((f32::from(w) * sx).round() as u32).max(1),
            ((f32::from(h) * sy).round() as u32).max(1),
        ))
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
        window.remove_window();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| open_crop_session(state, cx));
        })
        .detach();
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
        let height = RESOLUTIONS[self.res_ix].1;
        let mic = self.mics[self.mic_ix].0.clone();
        let crop = if self.crop_session {
            match self.sel_to_stream(window.viewport_size()) {
                Some(c) => Some(c),
                None => return, // nothing selected yet — draw a region first
            }
        } else if self.scope == Scope::Crop {
            // Crop armed without a session (edge case) — enter one instead.
            self.enter_crop(window, cx);
            return;
        } else {
            None
        };
        window.remove_window();
        match (self.mode, crop) {
            (Mode::Screenshot, None) => {
                capture_then_overlay(overlay::OverlayStart::PreviewFull, cx)
            }
            (Mode::Screenshot, Some(rect)) => {
                capture_then_overlay(overlay::OverlayStart::PreviewRegion(rect), cx)
            }
            (Mode::Record, crop) => start_recording_flow(crop, Some(height), mic, self.system_audio, cx),
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
            match self.sel_to_stream(viewport) {
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
            for ix in 0..RESOLUTIONS.len() {
                let (label, ..) = RESOLUTIONS[ix];
                let label = if ix == 2 { "2K" } else { label };
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
        }

        pill.child(div().flex_1()).child(
            div()
                .id("fire")
                .w(px(40.))
                .h(px(40.))
                .flex_none()
                .rounded_full()
                .cursor(CursorStyle::PointingHand)
                .bg(gpui::rgb(0xff3b30))
                .border_2()
                .border_color(gpui::rgb(0xffffff))
                .hover(|s| s.bg(gpui::rgb(0xff5b4d)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| this.fire(window, cx)),
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
                    "escape" => cx.quit(),
                    "enter" => this.fire(window, cx),
                    _ => {}
                }
            }));

        if self.crop_session {
            root = root
                .cursor(CursorStyle::Crosshair)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.dragging = true;
                        this.sel_start = Some(ev.position);
                        this.sel_end = Some(ev.position);
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                    if this.dragging {
                        this.sel_end = Some(ev.position);
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                        if this.dragging {
                            this.dragging = false;
                            this.sel_end = Some(ev.position);
                            cx.notify();
                        }
                    }),
                );

            // Light scrim so the live desktop stays visible; the selection is
            // a hole in it (four slabs).
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
                }
            }
            // Pill floats near the top, above the scrim.
            root = root.child(div().mt(px(24.)).child(pill));
            if self.mic_open && record {
                root = root.child(self.mic_dropdown(cx));
            }
            if self.sel_rect().is_none() {
                root = root.child(
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
                        .child("Drag to choose the area · ● captures · Esc back"),
                );
            }
        } else {
            root = root.child(div().mt_2().child(pill));
            if self.mic_open && record {
                root = root.child(self.mic_dropdown(cx));
            }
        }

        root
    }
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
                })
            })
            .await;
        match started {
            Ok(recording) => cx.update(|cx| recorder::open_window(recording, cx)),
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
