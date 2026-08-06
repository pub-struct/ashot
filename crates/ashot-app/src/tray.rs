//! System-tray icon (StatusNotifierItem) shown while recording.
//!
//! Wayland has no per-window capture exclusion, so the pill can be hidden
//! (minimized) during a recording — the tray icon is then the way to pause
//! or stop without the controls appearing in the capture. Left-clicking the
//! icon re-raises the pill window.
//!
//! Note: KDE/most bars show SNI icons natively; GNOME needs the
//! "AppIndicator and KStatusNotifierItem" shell extension. If no tray host
//! is present, spawning fails and we just run without an icon.

use std::sync::mpsc::Sender;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Status, Tray};

/// Commands from the tray thread to the recorder view (drained by a pump
/// on the UI side — ksni invokes menu callbacks on its own service thread).
pub enum TrayCmd {
    TogglePause,
    Stop,
    Show,
}

pub struct RecordTray {
    pub paused: bool,
    can_control: bool,
    tx: Sender<TrayCmd>,
}

impl Tray for RecordTray {
    fn id(&self) -> String {
        "ashot-recorder".into()
    }

    fn title(&self) -> String {
        if self.paused { "ashot — recording paused".into() } else { "ashot — recording".into() }
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_name(&self) -> String {
        if self.paused { "media-playback-pause".into() } else { "media-record".into() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayCmd::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        if self.can_control {
            items.push(
                StandardItem {
                    label: if self.paused { "Resume".into() } else { "Pause".into() },
                    icon_name: if self.paused {
                        "media-playback-start".into()
                    } else {
                        "media-playback-pause".into()
                    },
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(TrayCmd::TogglePause);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: "Stop & save".into(),
                icon_name: "media-playback-stop".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::Stop);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Show controls".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::Show);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

/// Spawn the tray icon; `None` when no StatusNotifier host is available
/// (e.g. GNOME without the AppIndicator extension) — recording works fine
/// without it, there's just no top-bar entry.
pub fn spawn(can_control: bool, tx: Sender<TrayCmd>) -> Option<ksni::blocking::Handle<RecordTray>> {
    use ksni::blocking::TrayMethods;
    RecordTray { paused: false, can_control, tx }.spawn().ok()
}
