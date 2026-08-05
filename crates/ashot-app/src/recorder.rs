//! Small movable status window shown while a recording runs:
//! red dot, elapsed time, encoder badge, Stop.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Bounds, Context, CursorStyle, FocusHandle, Focusable, KeyDownEvent,
    MouseButton, MouseDownEvent, SharedString, Size, Window, WindowBounds, WindowOptions,
};

use ashot_core::record::Recording;

use crate::theme;

pub fn open_window(recording: Recording, cx: &mut App) {
    let bounds = Bounds::centered(None, Size { width: px(320.), height: px(120.) }, cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(SharedString::from("ashot — recording")),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| RecorderView::new(recording, cx));
            let handle = view.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
            // Titlebar ✕ = Stop: finalize instead of orphaning the encoder.
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
    elapsed_s: u64,
    stopping: bool,
    focus_handle: FocusHandle,
}

impl RecorderView {
    fn new(recording: Recording, cx: &mut Context<Self>) -> Self {
        let encoder = recording.encoder;
        // Test hook: stop automatically after N seconds.
        let auto_stop: Option<u64> = std::env::var("ASHOT_TEST_AUTOSTOP")
            .ok()
            .and_then(|v| v.parse().ok());
        // 1s tick: refresh elapsed, watch for encoder death.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this: &mut Self, cx| {
                    if this.stopping {
                        return false;
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
            elapsed_s: 0,
            stopping: false,
            focus_handle: cx.focus_handle(),
        }
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
        let encoder_label = if self.encoder.starts_with("va") { "GPU" } else { "CPU" };

        div()
            .id("recorder")
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme::bg())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" || ev.keystroke.key == "enter" {
                    this.stop(cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(10.)).h(px(10.)).rounded_full().bg(gpui::rgb(0xff3b30)))
                    .child(
                        div()
                            .text_lg()
                            .text_color(theme::text())
                            .child(if self.stopping {
                                SharedString::from("Finalizing…")
                            } else {
                                SharedString::from(elapsed)
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme::surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(format!("{encoder_label} · {}", self.encoder)),
                    ),
            )
            .child(
                div()
                    .id("stop")
                    .py_2()
                    .flex()
                    .justify_center()
                    .rounded_md()
                    .cursor(CursorStyle::PointingHand)
                    .bg(theme::accent())
                    .text_color(gpui::rgb(0xffffff))
                    .text_sm()
                    .hover(|s| s.bg(theme::accent_hover()))
                    .child(if self.stopping { "Saving…" } else { "■ Stop recording" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| this.stop(cx)),
                    ),
            )
    }
}
