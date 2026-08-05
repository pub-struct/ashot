//! Recording status pill (user-sketched design):
//!
//!   [● TIME] [🎤 mute/unmute] [⏸ pause/resume] [■ Stop] [⣿ grab area]
//!
//! Pause/mute talk to the GStreamer controller subprocess; the grab area
//! starts a compositor window move (the pill window has no titlebar).

use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Bounds, Context, CursorStyle, FocusHandle, Focusable, KeyDownEvent,
    MouseButton, MouseDownEvent, SharedString, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowOptions,
};

use ashot_core::record::Recording;

use crate::theme;

pub fn open_window(recording: Recording, cx: &mut App) {
    let bounds = Bounds::centered(None, Size { width: px(430.), height: px(72.) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            window_background: WindowBackgroundAppearance::Transparent,
            is_resizable: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| RecorderView::new(recording, cx));
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            // Closing the pill = Stop: finalize instead of orphaning.
            let close_view = view.clone();
            window.on_window_should_close(cx, move |_, cx| {
                close_view.update(cx, |this, cx| this.stop(cx));
                false // stop() quits the app once the file is finalized
            });
            view
        },
    );
    if window.is_err() {
        eprintln!("failed to open recorder window");
        cx.quit();
    }
}

struct RecorderView {
    recording: Option<Recording>,
    encoder: &'static str,
    has_audio: bool,
    can_control: bool,
    elapsed_s: u64,
    stopping: bool,
    focus_handle: FocusHandle,
}

impl RecorderView {
    fn new(recording: Recording, cx: &mut Context<Self>) -> Self {
        let encoder = recording.encoder;
        let has_audio = recording.has_audio;
        let can_control = recording.can_control();
        // Test hooks: ASHOT_TEST_AUTOSTOP=N stops after N recorded seconds;
        // ASHOT_TEST_PAUSE=A,B pauses at A and resumes at B (wall seconds).
        let auto_stop: Option<u64> =
            std::env::var("ASHOT_TEST_AUTOSTOP").ok().and_then(|v| v.parse().ok());
        let test_pause: Option<(u64, u64)> = std::env::var("ASHOT_TEST_PAUSE").ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        });

        cx.spawn(async move |this, cx| {
            let mut ticks: u64 = 0;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                ticks += 1;
                let alive = this.update(cx, |this: &mut Self, cx| {
                    if this.stopping {
                        return false;
                    }
                    if let Some((pause_at, resume_at)) = test_pause {
                        if ticks == pause_at || ticks == resume_at {
                            let _ = this.toggle_pause(cx);
                        }
                    }
                    if auto_stop.is_some_and(|limit| this.elapsed_s >= limit) {
                        this.stop(cx);
                        return false;
                    }
                    if let Some(rec) = this.recording.as_mut() {
                        this.elapsed_s = rec.elapsed().as_secs();
                        if !rec.is_running() {
                            eprintln!(
                                "{}",
                                serde_json::json!({ "ok": false, "error": {
                                    "code": "record",
                                    "message": "encoder process exited unexpectedly" } })
                            );
                            cx.quit();
                            return false;
                        }
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        Self {
            recording: Some(recording),
            encoder,
            has_audio,
            can_control,
            elapsed_s: 0,
            stopping: false,
            focus_handle: cx.focus_handle(),
        }
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if let Some(rec) = self.recording.as_mut() {
            if let Err(e) = rec.toggle_pause() {
                eprintln!(
                    "{}",
                    serde_json::json!({ "warning": format!("pause failed: {e}") })
                );
            }
        }
        cx.notify();
    }

    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if let Some(rec) = self.recording.as_mut() {
            let target = !rec.is_muted();
            if let Err(e) = rec.set_muted(target) {
                eprintln!(
                    "{}",
                    serde_json::json!({ "warning": format!("mute failed: {e}") })
                );
            }
        }
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let Some(recording) = self.recording.take() else { return };
        self.stopping = true;
        cx.notify();
        let encoder = recording.encoder;
        let (w, h) = (recording.out_width, recording.out_height);
        let elapsed = recording.elapsed().as_secs_f64();
        cx.spawn(async move |_, cx| {
            let stopped = cx.background_executor().spawn(async move { recording.stop() }).await;
            match stopped {
                Ok(path) => println!(
                    "{}",
                    serde_json::json!({
                        "ok": true, "action": "record",
                        "path": path.display().to_string(),
                        "encoder": encoder, "width": w, "height": h,
                        "duration_s": (elapsed * 10.0).round() / 10.0,
                    })
                ),
                Err(e) => eprintln!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } })
                ),
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    fn chip(
        &self,
        id: &'static str,
        label: SharedString,
        active: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_3()
            .py_1()
            .flex_none()
            .rounded_full()
            .cursor(CursorStyle::PointingHand)
            .bg(theme::surface())
            .border_1()
            .border_color(if active { theme::accent() } else { theme::border() })
            .text_sm()
            .text_color(if active { theme::text() } else { theme::text_muted() })
            .hover(|s| s.bg(theme::surface_hover()))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| handler(this, cx)),
            )
    }
}

impl Focusable for RecorderView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RecorderView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let elapsed = format!(
            "{:02}:{:02}:{:02}",
            self.elapsed_s / 3600,
            (self.elapsed_s % 3600) / 60,
            self.elapsed_s % 60
        );
        let paused = self.recording.as_ref().is_some_and(|r| r.is_paused());
        let muted = self.recording.as_ref().is_some_and(|r| r.is_muted());

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
            // ● TIME
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(
                        div().w(px(10.)).h(px(10.)).rounded_full().bg(if paused {
                            gpui::rgb(0x8e8e93)
                        } else {
                            gpui::rgb(0xff3b30)
                        }),
                    )
                    .child(
                        div()
                            .text_lg()
                            .text_color(if paused { theme::text_muted() } else { theme::text() })
                            .child(if self.stopping {
                                SharedString::from("Saving…")
                            } else {
                                SharedString::from(elapsed)
                            }),
                    ),
            );

        // 🎤 mute/unmute (only when recording audio and controllable)
        if self.has_audio && self.can_control {
            pill = pill.child(self.chip(
                "mute",
                if muted { "🎤 Muted".into() } else { "🎤 On".into() },
                muted,
                cx,
                |this, cx| this.toggle_mute(cx),
            ));
        }

        // ⏸ pause/resume
        if self.can_control {
            pill = pill.child(self.chip(
                "pause",
                if paused { "▶ Resume".into() } else { "⏸ Pause".into() },
                paused,
                cx,
                |this, cx| this.toggle_pause(cx),
            ));
        }

        // ■ Stop
        pill = pill
            .child(
                div()
                    .id("stop")
                    .px_4()
                    .py_1p5()
                    .flex_none()
                    .rounded_full()
                    .cursor(CursorStyle::PointingHand)
                    .bg(theme::accent())
                    .text_color(gpui::rgb(0xffffff))
                    .text_sm()
                    .hover(|s| s.bg(theme::accent_hover()))
                    .child(if self.stopping { "Saving…" } else { "■ Stop" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| this.stop(cx)),
                    ),
            )
            // ⣿ grab area — drag to move the pill anywhere.
            .child(
                div()
                    .id("grab")
                    .px_2()
                    .py_1()
                    .flex_none()
                    .rounded_md()
                    .cursor(CursorStyle::OpenHand)
                    .text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::surface_hover()))
                    .child("⣿")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _: &MouseDownEvent, window, _| {
                            window.start_window_move();
                        }),
                    ),
            );

        div()
            .id("recorder")
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" | "enter" => this.stop(cx),
                    "space" => this.toggle_pause(cx),
                    "m" => this.toggle_mute(cx),
                    _ => {}
                }
            }))
            .child(pill)
    }
}
