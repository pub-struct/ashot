//! Recording control.
//!
//! While recording, the primary UI is the system-tray icon (see
//! `crate::tray`): pause/stop live in its menu, so nothing of the app is
//! visible in the capture (Wayland/X11 have no per-window capture
//! exclusion, unlike Windows/macOS).
//!
//! The status pill window
//!
//!   [● TIME] [🎤 mute/unmute] [⏸ pause/resume] [■ Stop] [🫥 hide] [⣿ grab]
//!
//! is NOT shown when recording starts — it opens on demand via the tray's
//! "Show controls" (or a left-click on the icon) and can be dismissed again
//! with 🫥. Only when no StatusNotifier host exists (e.g. GNOME without the
//! AppIndicator extension) does the pill open right away, since it is then
//! the only way to stop; in that fallback 🫥 minimizes instead of closing.
//!
//! `RecorderController` is an app-level (windowless) entity owning the
//! recording, the tray handle, and the 1s ticker; the pill is a thin view
//! over it.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, App, Bounds, Context, CursorStyle, Entity, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Size, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowOptions,
};

use ashot_core::record::Recording;

use crate::theme;
use crate::tray::TrayCmd;

pub fn start(recording: Recording, cx: &mut App) {
    // ksni runs menu callbacks on its own service thread; a channel plus a
    // pump on the UI side keeps the gpui entity single-threaded.
    let (tray_tx, tray_rx) = std::sync::mpsc::channel::<TrayCmd>();
    let tray = crate::tray::spawn(recording.can_control(), tray_tx);
    let has_tray = tray.is_some();

    let ctrl = cx.new(|cx| RecorderController::new(recording, tray, cx));

    if has_tray {
        let pump = ctrl.clone();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                let alive = pump.update(cx, |this, cx| {
                    while let Ok(cmd) = tray_rx.try_recv() {
                        match cmd {
                            TrayCmd::TogglePause => this.toggle_pause(cx),
                            TrayCmd::Stop => this.stop(cx),
                            TrayCmd::Show => this.show_pill(cx),
                        }
                    }
                    !this.stopping
                });
                if !alive {
                    break;
                }
            }
        })
        .detach();
    } else {
        // No top-bar host: the pill is the only control surface.
        ctrl.update(cx, |this, cx| this.show_pill(cx));
    }
}

struct RecorderController {
    recording: Option<Recording>,
    has_audio: bool,
    can_control: bool,
    elapsed_s: u64,
    stopping: bool,
    tray: Option<ksni::blocking::Handle<crate::tray::RecordTray>>,
    pill: Option<WindowHandle<RecorderView>>,
}

impl RecorderController {
    fn new(
        recording: Recording,
        tray: Option<ksni::blocking::Handle<crate::tray::RecordTray>>,
        cx: &mut Context<Self>,
    ) -> Self {
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
                            this.toggle_pause(cx);
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
            has_audio,
            can_control,
            elapsed_s: 0,
            stopping: false,
            tray,
            pill: None,
        }
    }

    fn is_paused(&self) -> bool {
        self.recording.as_ref().is_some_and(|r| r.is_paused())
    }

    fn is_muted(&self) -> bool {
        self.recording.as_ref().is_some_and(|r| r.is_muted())
    }

    /// Open the status pill, or raise it if it's already open.
    fn show_pill(&mut self, cx: &mut Context<Self>) {
        if let Some(pill) = self.pill {
            if pill.update(cx, |_, window, _| window.activate_window()).is_ok() {
                return;
            }
            self.pill = None;
        }
        let ctrl = cx.entity();
        let bounds = Bounds::centered(None, Size { width: px(430.), height: px(72.) }, cx);
        let pill = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                window_background: WindowBackgroundAppearance::Transparent,
                is_resizable: false,
                ..Default::default()
            },
            |window, cx| {
                // Deliberately no focus grab: the pill is a passive status
                // window and shouldn't steal focus from whatever the user
                // is recording. Keys work once the user clicks it.
                let view = cx.new(|cx| RecorderView::new(ctrl.clone(), cx));
                let close_ctrl = ctrl.clone();
                window.on_window_should_close(cx, move |_, cx| {
                    close_ctrl.update(cx, |this, cx| this.pill_should_close(cx))
                });
                view
            },
        );
        match pill {
            Ok(pill) => self.pill = Some(pill),
            Err(_) => eprintln!("failed to open recorder window"),
        }
    }

    /// With a tray, closing the pill just dismisses it (recording keeps
    /// going); without one it's the only control, so closing means Stop.
    fn pill_should_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.tray.is_some() && !self.stopping {
            self.pill = None;
            true
        } else {
            self.stop(cx);
            false // stop() quits the app once the file is finalized
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
        let paused = self.is_paused();
        if let Some(tray) = &self.tray {
            tray.update(|t| t.paused = paused);
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
        if let Some(tray) = self.tray.take() {
            let _ = tray.shutdown();
        }
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

struct RecorderView {
    ctrl: Entity<RecorderController>,
    focus_handle: FocusHandle,
}

impl RecorderView {
    fn new(ctrl: Entity<RecorderController>, cx: &mut Context<Self>) -> Self {
        cx.observe(&ctrl, |_, _, cx| cx.notify()).detach();
        Self { ctrl, focus_handle: cx.focus_handle() }
    }

    /// 🫥 — with a tray the pill closes outright (re-open from the top
    /// bar); without one it only minimizes, so it stays reachable from the
    /// taskbar/overview.
    fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ctrl.read(cx).tray.is_some() {
            self.ctrl.update(cx, |this, _| this.pill = None);
            window.remove_window();
        } else {
            window.minimize_window();
        }
    }

    fn chip(
        &self,
        id: &'static str,
        label: SharedString,
        active: bool,
        cx: &mut Context<Self>,
        handler: impl Fn(&mut RecorderController, &mut Context<RecorderController>) + 'static,
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
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.ctrl.update(cx, |ctrl, cx| handler(ctrl, cx));
                }),
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
        let (elapsed_s, stopping, has_audio, can_control, paused, muted) = {
            let c = self.ctrl.read(cx);
            (c.elapsed_s, c.stopping, c.has_audio, c.can_control, c.is_paused(), c.is_muted())
        };
        let elapsed = format!(
            "{:02}:{:02}:{:02}",
            elapsed_s / 3600,
            (elapsed_s % 3600) / 60,
            elapsed_s % 60
        );

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
                            .child(if stopping {
                                SharedString::from("Saving…")
                            } else {
                                SharedString::from(elapsed)
                            }),
                    ),
            );

        // 🎤 mute/unmute (only when recording audio and controllable)
        if has_audio && can_control {
            pill = pill.child(self.chip(
                "mute",
                if muted { "🎤 Muted".into() } else { "🎤 On".into() },
                muted,
                cx,
                |ctrl, cx| ctrl.toggle_mute(cx),
            ));
        }

        // ⏸ pause/resume
        if can_control {
            pill = pill.child(self.chip(
                "pause",
                if paused { "▶ Resume".into() } else { "⏸ Pause".into() },
                paused,
                cx,
                |ctrl, cx| ctrl.toggle_pause(cx),
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
                    .child(if stopping { "Saving…" } else { "■ Stop" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.ctrl.update(cx, |ctrl, cx| ctrl.stop(cx));
                        }),
                    ),
            )
            // 🫥 hide — dismiss the pill so it doesn't appear in the
            // recording; bring it back from the tray icon.
            .child(
                div()
                    .id("hide")
                    .px_3()
                    .py_1()
                    .flex_none()
                    .rounded_full()
                    .cursor(CursorStyle::PointingHand)
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::border())
                    .text_sm()
                    .text_color(theme::text_muted())
                    .hover(|s| s.bg(theme::surface_hover()))
                    .child("🫥 Hide")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.hide(window, cx);
                        }),
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
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" | "enter" => this.ctrl.update(cx, |ctrl, cx| ctrl.stop(cx)),
                    "space" => this.ctrl.update(cx, |ctrl, cx| ctrl.toggle_pause(cx)),
                    "m" => this.ctrl.update(cx, |ctrl, cx| ctrl.toggle_mute(cx)),
                    "h" => this.hide(window, cx),
                    _ => {}
                }
            }))
            .child(pill)
    }
}
