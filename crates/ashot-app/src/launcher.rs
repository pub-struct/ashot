//! The `ashot ui` entry point: a small floating toolbar to pick
//! Screenshot / Record × Full / Crop (+ resolution for recordings).

use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Application, Bounds, Context, CursorStyle, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Size, Window, WindowBounds,
    WindowOptions,
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
                start_recording_flow(None, Some(1080), cx);
            } else {
                open_window(cx);
            }
            cx.activate(true);
        });
    Ok(())
}

pub fn open_window(cx: &mut App) {
    let bounds = Bounds::centered(None, Size { width: px(400.), height: px(200.) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(SharedString::from("ashot")),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| LauncherView::new(window, cx));
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            // Explicit quit mode: titlebar ✕ must end the app itself.
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
enum Mode {
    Screenshot,
    Record,
}

#[derive(Clone, Copy)]
enum Scope {
    Full,
    Crop,
}

struct LauncherView {
    mode: Mode,
    res_ix: usize,
    focus_handle: FocusHandle,
}

impl LauncherView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Test hook: start in record mode for automated visual checks.
        let mode = if std::env::var_os("ASHOT_TEST_MODE").is_some_and(|v| v == "record") {
            Mode::Record
        } else {
            Mode::Screenshot
        };
        // Test hook: trigger the Full action through the real click path
        // (window close → async flow) one second after opening.
        if std::env::var_os("ASHOT_TEST_MODE").is_some_and(|v| v == "auto-full") {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                this.update_in(cx, |this, window, cx| this.go(Scope::Full, window, cx))
                    .ok();
            })
            .detach();
        }
        Self { mode, res_ix: 1, focus_handle: cx.focus_handle() }
    }

    fn go(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.mode;
        let height = RESOLUTIONS[self.res_ix].1;
        window.remove_window();
        match (mode, scope) {
            (Mode::Screenshot, Scope::Full) => {
                capture_then_overlay(overlay::OverlayStart::PreviewFull, cx)
            }
            (Mode::Screenshot, Scope::Crop) => capture_then_overlay(
                overlay::OverlayStart::Select(overlay::Purpose::Screenshot),
                cx,
            ),
            (Mode::Record, Scope::Full) => start_recording_flow(None, Some(height), cx),
            (Mode::Record, Scope::Crop) => capture_then_overlay(
                overlay::OverlayStart::Select(overlay::Purpose::Record { height }),
                cx,
            ),
        }
    }

    fn segment(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex_1()
            .py_1p5()
            .flex()
            .justify_center()
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

    fn action_button(
        &self,
        id: &'static str,
        label: &'static str,
        scope: Scope,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .flex_1()
            .py_2()
            .flex()
            .justify_center()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .text_sm()
            .text_color(theme::text())
            .hover(|s| s.bg(theme::surface_hover()).border_color(theme::accent()))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    this.go(scope, window, cx)
                }),
            )
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
        div()
            .id("launcher")
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme::bg())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    cx.quit();
                }
            }))
            .child(
                // Mode toggle.
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .p_1()
                    .rounded_lg()
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::border())
                    .child(self.segment("mode-shot", "Screenshot", !record, cx, |this, cx| {
                        this.mode = Mode::Screenshot;
                        cx.notify();
                    }))
                    .child(self.segment("mode-rec", "⏺ Record", record, cx, |this, cx| {
                        this.mode = Mode::Record;
                        cx.notify();
                    })),
            )
            .child(
                // Scope actions.
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(self.action_button(
                        "full",
                        if record { "Record full screen" } else { "Full screen" },
                        Scope::Full,
                        cx,
                    ))
                    .child(self.action_button(
                        "crop",
                        if record { "Record region" } else { "Select region" },
                        Scope::Crop,
                        cx,
                    )),
            )
            .when(record, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::text_muted())
                                .mr_1()
                                .child("Resolution"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_1()
                                .gap_1()
                                .p_1()
                                .rounded_lg()
                                .bg(theme::surface())
                                .border_1()
                                .border_color(theme::border())
                                .children((0..RESOLUTIONS.len()).map(|ix| {
                                    let (label, ..) = RESOLUTIONS[ix];
                                    let active = self.res_ix == ix;
                                    div()
                                        .id(("res", ix))
                                        .flex_1()
                                        .py_1()
                                        .flex()
                                        .justify_center()
                                        .rounded_md()
                                        .cursor(CursorStyle::PointingHand)
                                        .text_xs()
                                        .when(active, |d| {
                                            d.bg(theme::accent()).text_color(gpui::rgb(0xffffff))
                                        })
                                        .when(!active, |d| {
                                            d.text_color(theme::text_muted())
                                                .hover(|s| s.bg(theme::surface_hover()))
                                        })
                                        .child(label)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                                this.res_ix = ix;
                                                cx.notify();
                                            }),
                                        )
                                })),
                        ),
                )
            })
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
pub fn start_recording_flow(crop: Option<(u32, u32, u32, u32)>, height: Option<u32>, cx: &mut App) {
    cx.spawn(async move |cx| {
        // Let our windows unmap so they aren't in the recording's first frames.
        cx.background_executor().timer(Duration::from_millis(400)).await;
        let started = cx
            .background_executor()
            .spawn(async move {
                ashot_core::record::start_recording(RecordOptions { output: None, height, crop })
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
