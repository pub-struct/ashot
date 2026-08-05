//! GPU screen recording: ScreenCast portal → PipeWire → gst-launch VA-API.
//!
//! Frames come from the compositor as DMA-BUF (GPU memory); `vapostproc`
//! converts/scales on the GPU and `vah264enc` feeds the hardware encoder —
//! the CPU never touches pixels. Encoding runs in a `gst-launch-1.0 -e`
//! subprocess (no GStreamer linkage; `-e` finalizes the MP4 on SIGINT).
//!
//! The portal session lives on a keeper thread for the whole recording —
//! dropping it would end the stream.

use std::io::Write;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;

use crate::error::{Error, Result};

/// (label, target height, H.264 bitrate in kbps) — bitrates tuned for screen content.
pub const RESOLUTIONS: &[(&str, u32, u32)] =
    &[("720p", 720, 6_000), ("1080p", 1080, 10_000), ("1440p", 1440, 16_000)];

/// The pipewire fd is dup2'd onto this number in the child.
const CHILD_FD: i32 = 3;

#[derive(Debug, Clone, Default)]
pub struct RecordOptions {
    /// Defaults to ~/Videos/Screencasts/rec-<timestamp>.mp4
    pub output: Option<PathBuf>,
    /// Target output height (720/1080/1440); None = native. Never upscales.
    pub height: Option<u32>,
    /// Crop region in stream pixels (x, y, w, h), applied before scaling.
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Microphone: None = no audio, Some("default") = default input,
    /// Some(name) = a specific PulseAudio/PipeWire source name.
    pub mic: Option<String>,
}

pub struct Recording {
    child: Child,
    stop_keeper: mpsc::Sender<()>,
    pub path: PathBuf,
    pub encoder: &'static str,
    pub out_width: u32,
    pub out_height: u32,
    pub has_audio: bool,
    started: Instant,
}

struct PortalStream {
    fd: OwnedFd,
    node_id: u32,
    width: u32,
    height: u32,
}

pub fn start_recording(opts: RecordOptions) -> Result<Recording> {
    let path = match opts.output.clone() {
        Some(p) => p,
        None => crate::paths::default_record_path()?,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let (keeper, stream) = open_portal_stream()?;

    // Base frame = crop region or the whole stream.
    let (bw, bh) = match opts.crop {
        Some((_, _, w, h)) => (w.min(stream.width), h.min(stream.height)),
        None => (stream.width, stream.height),
    };
    // Scale to the requested height (never up), even dimensions for NV12/H.264.
    let target_h = opts.height.map_or(bh, |h| h.min(bh));
    let scale = target_h as f64 / bh as f64;
    let out_w = even((bw as f64 * scale).round() as u32);
    let out_h = even(target_h);
    let bitrate = RESOLUTIONS
        .iter()
        .find(|(_, h, _)| *h >= out_h)
        .map_or(16_000, |(_, _, kbps)| *kbps);

    let gpu = gst_has_element("vah264enc");
    let (encoder, mut child) = spawn_gst(
        &stream, &opts, &path, out_w, out_h, bitrate,
        if gpu { "vah264enc" } else { "x264enc" },
    )?;

    // If the GPU pipeline dies immediately (format negotiation, driver quirks),
    // fall back to CPU encoding once rather than failing the recording.
    std::thread::sleep(Duration::from_millis(1200));
    let (encoder, child) = match child.try_wait() {
        Ok(Some(status)) if encoder == "vah264enc" => {
            eprintln!(
                "{}",
                serde_json::json!({
                    "warning": format!("GPU pipeline exited at startup ({status}); falling back to CPU x264"),
                })
            );
            // Portal fd was consumed by the dead child; need a fresh stream.
            drop(keeper);
            let (keeper2, stream2) = open_portal_stream()?;
            let (enc, child2) =
                spawn_gst(&stream2, &opts, &path, out_w, out_h, bitrate, "x264enc")?;
            return finish_start(keeper2, child2, enc, path, out_w, out_h, opts.mic.is_some());
        }
        Ok(Some(status)) => {
            return Err(Error::Record(format!("encoder exited at startup: {status}")));
        }
        _ => (encoder, child),
    };

    finish_start(keeper, child, encoder, path, out_w, out_h, opts.mic.is_some())
}

fn finish_start(
    keeper: mpsc::Sender<()>,
    child: Child,
    encoder: &'static str,
    path: PathBuf,
    out_width: u32,
    out_height: u32,
    has_audio: bool,
) -> Result<Recording> {
    Ok(Recording {
        child,
        stop_keeper: keeper,
        path,
        encoder,
        out_width,
        out_height,
        has_audio,
        started: Instant::now(),
    })
}

impl Drop for Recording {
    /// Last-resort guard: if the handle is dropped without `stop()` (window
    /// closed, panic), interrupt the encoder so it finalizes and exits
    /// instead of recording forever as an orphan.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGINT);
            }
        }
    }
}

impl Recording {
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// True while the encoder subprocess is alive.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// SIGINT the pipeline (gst-launch -e turns it into EOS → valid MP4),
    /// wait for finalization, release the portal session.
    pub fn stop(mut self) -> Result<PathBuf> {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() > deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let _ = self.stop_keeper.send(());
        if !self.path.exists() {
            return Err(Error::Record("recording file was not produced".into()));
        }
        Ok(self.path.clone())
    }
}

/// Portal handshake on a keeper thread; returns once the stream is live.
/// The thread holds the session until told to stop.
fn open_portal_stream() -> Result<(mpsc::Sender<()>, PortalStream)> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    std::thread::spawn(move || {
        let result = futures::executor::block_on(async {
            let proxy = Screencast::new().await?;
            let session = proxy.create_session().await?;
            let token = load_restore_token();
            proxy
                .select_sources(
                    &session,
                    CursorMode::Embedded,
                    SourceType::Monitor.into(),
                    false,
                    token.as_deref(),
                    PersistMode::ExplicitlyRevoked,
                )
                .await?
                .response()?;
            let streams = proxy.start(&session, None).await?.response()?;
            if let Some(new_token) = streams.restore_token() {
                save_restore_token(new_token);
            }
            let first = streams.streams().first().ok_or(ashpd::Error::NoResponse)?;
            let node_id = first.pipe_wire_node_id();
            let (w, h) = first.size().unwrap_or((1920, 1080));
            let fd = proxy.open_pipe_wire_remote(&session).await?;
            Ok::<_, ashpd::Error>((
                session,
                PortalStream { fd, node_id, width: w.max(1) as u32, height: h.max(1) as u32 },
            ))
        });

        match result {
            Ok((session, stream)) => {
                let _ = ready_tx.send(Ok(stream));
                // Keep the session alive until stop (or handle drop).
                let _ = stop_rx.recv();
                drop(session);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });

    let stream = ready_rx
        .recv()
        .map_err(|_| Error::Record("portal thread died".into()))?
        .map_err(map_portal_err)?;
    Ok((stop_tx, stream))
}

fn map_portal_err(err: ashpd::Error) -> Error {
    match &err {
        ashpd::Error::Response(_) => {
            Error::Record("screen recording was denied or cancelled in the portal dialog".into())
        }
        _ => Error::Record(format!("screencast portal: {err}")),
    }
}

fn spawn_gst(
    stream: &PortalStream,
    opts: &RecordOptions,
    path: &std::path::Path,
    out_w: u32,
    out_h: u32,
    bitrate_kbps: u32,
    encoder: &'static str,
) -> Result<(&'static str, Child)> {
    // Build one launch-line, then split into argv words (no shell involved).
    let mut line = format!(
        "pipewiresrc fd={CHILD_FD} path={} do-timestamp=true ! queue max-size-buffers=8 ",
        stream.node_id
    );
    if let Some((x, y, w, h)) = opts.crop {
        let right = stream.width.saturating_sub(x + w);
        let bottom = stream.height.saturating_sub(y + h);
        line += &format!("! videocrop left={x} top={y} right={right} bottom={bottom} ");
    }
    if encoder == "vah264enc" {
        line += &format!(
            "! vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width={out_w},height={out_h} \
             ! vah264enc bitrate={bitrate_kbps} ! h264parse "
        );
    } else {
        line += &format!(
            "! videoconvert ! videoscale ! video/x-raw,format=I420,width={out_w},height={out_h} \
             ! x264enc bitrate={bitrate_kbps} speed-preset=veryfast tune=zerolatency key-int-max=120 \
             ! h264parse "
        );
    }
    line += "! mux. ";
    line += &format!("mp4mux name=mux ! filesink location={} ", shell_safe(path)?);
    if let Some(mic) = &opts.mic {
        if let Some(aac) = ["avenc_aac", "voaacenc"].iter().find(|e| gst_has_element(e)) {
            let device = if mic == "default" { String::new() } else { format!(" device={mic}") };
            line += &format!(
                "pulsesrc{device} ! queue ! audioconvert ! audioresample \
                 ! {aac} bitrate=128000 ! aacparse ! mux. "
            );
        } else {
            eprintln!(
                "{}",
                serde_json::json!({ "warning": "no AAC encoder found; recording without audio" })
            );
        }
    }

    if std::env::var_os("ASHOT_DEBUG").is_some() {
        eprintln!("[ashot debug] pipeline: {line}");
    }
    let mut args: Vec<String> = vec!["-e".into(), "-q".into()];
    args.extend(split_launch_line(&line));

    let raw_fd = stream.fd.try_clone().map_err(|e| Error::Record(e.to_string()))?.into_raw_fd();
    let mut cmd = Command::new("gst-launch-1.0");
    cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(move || {
            // dup2 clears O_CLOEXEC so the child inherits the pipewire fd.
            if libc::dup2(raw_fd, CHILD_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| Error::Record(format!("gst-launch-1.0 not runnable: {e}")))?;
    // The child holds its dup; release the parent's copy.
    unsafe { libc::close(raw_fd) };
    Ok((encoder, child))
}

/// gst-launch takes the pipeline as argv words; caps strings must stay intact.
fn split_launch_line(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

fn shell_safe(path: &std::path::Path) -> Result<String> {
    let s = path.to_str().ok_or_else(|| Error::Record("non-UTF8 output path".into()))?;
    if s.contains(char::is_whitespace) {
        // gst-launch property parsing would split it; escape with quotes.
        Ok(format!("\"{s}\""))
    } else {
        Ok(s.to_string())
    }
}

fn even(v: u32) -> u32 {
    (v.max(2)) & !1
}

/// Microphones (non-monitor PulseAudio/PipeWire sources): (name, description).
/// Best-effort — returns empty if pactl is unavailable.
pub fn list_microphones() -> Vec<(String, String)> {
    let output = Command::new("pactl")
        .args(["--format=json", "list", "sources"])
        .output();
    let Ok(output) = output else { return Vec::new() };
    let Ok(sources) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    sources
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?;
            if name.ends_with(".monitor") {
                return None;
            }
            let desc = s.get("description").and_then(|d| d.as_str()).unwrap_or(name);
            Some((name.to_string(), desc.to_string()))
        })
        .collect()
}

pub fn gst_has_element(name: &str) -> bool {
    Command::new("gst-inspect-1.0")
        .arg("--exists")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn token_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ashot").join("screencast.token"))
}

fn load_restore_token() -> Option<String> {
    let path = token_path()?;
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn save_restore_token(token: &str) {
    if let Some(path) = token_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::File::create(&path).and_then(|mut f| f.write_all(token.as_bytes()));
    }
}
