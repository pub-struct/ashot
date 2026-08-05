//! The `ashot ui` entry point — a floating pill toolbar (user-sketched design):
//!
//!   [Screenshot|Record] │ [Full|Crop] ([720p|1080p|2K] [🎤 mic]) (●)
//!
//! One red button executes the selected combination. Record mode adds the
//! resolution selector and a microphone picker.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Application, Bounds, Context, CursorStyle, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowOptions,
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
                start_recording_flow(None, Some(1080), None, cx);
            } else {
                open_window(cx);
            }
            cx.activate(true);
        });
    Ok(())
}

pub fn open_window(cx: &mut App) {
    open_window_with(LauncherState::default(), cx);
}

/// Reopen the pill with restored selections (after a region pick).
pub fn open_window_with(state: LauncherState, cx: &mut App) {
    let bounds = Bounds::centered(None, Size { width: px(820.), height: px(280.) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            window_background: WindowBackgroundAppearance::Transparent,
            is_resizable: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| LauncherView::new(state.clone(), window, cx));
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

/// Snapshot of the pill's selections, carried through the crop-pick round
/// trip (pill closes → freeze-frame picker → pill reopens with the region).
#[derive(Clone)]
pub struct LauncherState {
    pub mode: Mode,
    pub scope: Scope,
    pub res_ix: usize,
    pub mic_ix: usize,
    /// Picked region in stream/image pixels.
    pub crop: Option<(u32, u32, u32, u32)>,
}

impl Default for LauncherState {
    fn default() -> Self {
        Self { mode: Mode::Screenshot, scope: Scope::Full, res_ix: 1, mic_ix: 0, crop: None }
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
    /// Region picked via the freeze-frame selector (stream pixels).
    crop: Option<(u32, u32, u32, u32)>,
    focus_handle: FocusHandle,
}

impl LauncherView {
    fn new(state: LauncherState, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            crop: state.crop,
            focus_handle: cx.focus_handle(),
        }
    }

    fn snapshot(&self) -> LauncherState {
        LauncherState {
            mode: self.mode,
            scope: self.scope,
            res_ix: self.res_ix,
            mic_ix: self.mic_ix,
            crop: self.crop,
        }
    }

    /// Crop segment: hide the pill and pick a region on a clean freeze-frame.
    fn start_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut state = self.snapshot();
        state.scope = Scope::Crop;
        window.remove_window();
        capture_then_overlay(
            overlay::OverlayStart::Select(overlay::Purpose::PickRegion { state }),
            cx,
        );
    }

    /// The red button: execute the selected combination. The region was
    /// already picked when Crop was selected; execution uses a fresh capture
    /// (screenshot) or the live stream (record) of that region.
    fn fire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let height = RESOLUTIONS[self.res_ix].1;
        let mic = self.mics[self.mic_ix].0.clone();
        if self.scope == Scope::Crop && self.crop.is_none() {
            // No region picked yet — pick first, then the user fires again.
            self.start_pick(window, cx);
            return;
        }
        window.remove_window();
        match (self.mode, self.scope) {
            (Mode::Screenshot, Scope::Full) => {
                capture_then_overlay(overlay::OverlayStart::PreviewFull, cx)
            }
            (Mode::Screenshot, Scope::Crop) => capture_then_overlay(
                overlay::OverlayStart::PreviewRegion(self.crop.expect("checked above")),
                cx,
            ),
            (Mode::Record, Scope::Full) => start_recording_flow(None, Some(height), mic, cx),
            (Mode::Record, Scope::Crop) => {
                start_recording_flow(self.crop, Some(height), mic, cx)
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            // Mode toggle.
            .child(
                self.seg_group()
                    .child(self.segment(("mode", 0), "Screenshot".into(), !record, cx, |this, _, cx| {
                        this.mode = Mode::Screenshot;
                        this.mic_open = false;
                        cx.notify();
                    }))
                    .child(self.segment(("mode", 1), "⏺ Record".into(), record, cx, |this, _, cx| {
                        this.mode = Mode::Record;
                        cx.notify();
                    })),
            )
            .child(div().w(px(1.)).h(px(24.)).bg(theme::border()))
            // Scope selector (both modes).
            .child(
                self.seg_group()
                    .child(self.segment(
                        ("scope", 0),
                        "Full".into(),
                        self.scope == Scope::Full,
                        cx,
                        |this, _, cx| {
                            this.scope = Scope::Full;
                            cx.notify();
                        },
                    ))
                    .child(self.segment(
                        ("scope", 1),
                        // Show the picked region; clicking re-picks.
                        match self.crop {
                            Some((_, _, w, h)) if self.scope == Scope::Crop => {
                                format!("Crop {w}×{h}").into()
                            }
                            _ => "Crop".into(),
                        },
                        self.scope == Scope::Crop,
                        cx,
                        |this, window, cx| this.start_pick(window, cx),
                    )),
            );

        if record {
            // Resolution selector.
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
                // Mic picker chip.
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
        }

        // The red button.
        pill = pill.child(div().flex_1()).child(
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
        );

        let mut root = div()
            .id("launcher")
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt_2()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => cx.quit(),
                    "enter" => this.fire(window, cx),
                    _ => {}
                }
            }))
            .child(pill);

        if self.mic_open && record {
            root = root.child(
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
                    })),
            );
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
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        // Let our windows unmap so they aren't in the recording's first frames.
        cx.background_executor().timer(Duration::from_millis(400)).await;
        let started = cx
            .background_executor()
            .spawn(async move {
                ashot_core::record::start_recording(RecordOptions { output: None, height, crop, mic })
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
