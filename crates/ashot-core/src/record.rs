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

/// (label, target height, H.264 bitrate in kbps). Screen content is mostly
/// sharp text/edges, which H.264 (especially the hardware encoders) smears at
/// "camera video" bitrates — so these run high; sizes stay reasonable because
/// static regions cost almost nothing.
pub const RESOLUTIONS: &[(&str, u32, u32)] =
    &[("720p", 720, 8_000), ("1080p", 1080, 16_000), ("1440p", 1440, 24_000)];

/// The pipewire fd is dup2'd onto this number in the child.
const CHILD_FD: i32 = 3;

/// GStreamer controller: runs the pipeline via python-gi and takes runtime
/// commands on stdin (pause/resume/mute/unmute/stop). SIGINT also → EOS.
/// Live-source pause is gap-free: GStreamer rebases running time on resume,
/// so paused wall-time never reaches the file.
const CONTROLLER_PY: &str = r#"
import sys, signal, time, gi
gi.require_version('Gst', '1.0')
from gi.repository import Gst, GLib

Gst.init(None)
pipeline = Gst.parse_launch(sys.argv[1])
loop = GLib.MainLoop()
exit_code = 0

def quit_loop():
    pipeline.set_state(Gst.State.NULL)
    loop.quit()

def on_msg(bus, msg):
    global exit_code
    if msg.type == Gst.MessageType.EOS:
        quit_loop()
    elif msg.type == Gst.MessageType.ERROR:
        err, dbg = msg.parse_error()
        src = msg.src.get_name() if msg.src else '?'
        print('gst-error: [' + src + '] ' + err.message + ' | ' + (dbg or ''),
              file=sys.stderr, flush=True)
        exit_code = 1
        quit_loop()

bus = pipeline.get_bus()
bus.add_signal_watch()
bus.connect('message', on_msg)

def send_eos(*_):
    pipeline.send_event(Gst.Event.new_eos())

signal.signal(signal.SIGINT, send_eos)
vol = pipeline.get_by_name('vol')

# Pause: probes on the gate elements drop buffers while paused and subtract
# the accumulated paused time from every later timestamp — the encoder sees
# one continuous timeline, so the paused stretch simply isn't in the file.
# The pipeline itself never leaves PLAYING (portal live sources dislike
# PAUSED), and stop-EOS always runs on a flowing pipeline.
paused = False
pause_started = 0
gap = 0  # ns

def make_probe():
    def probe(_pad, info):
        buf = info.get_buffer()
        if buf is None:
            return Gst.PadProbeReturn.OK
        if paused:
            return Gst.PadProbeReturn.DROP
        if gap:
            if buf.pts != Gst.CLOCK_TIME_NONE:
                buf.pts = max(0, buf.pts - gap)
            if buf.dts != Gst.CLOCK_TIME_NONE:
                buf.dts = max(0, buf.dts - gap)
        return Gst.PadProbeReturn.OK
    return probe

for name in ('vgate', 'agate'):
    el = pipeline.get_by_name(name)
    if el:
        el.get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, make_probe())

def set_paused(p):
    global paused, pause_started, gap
    if p and not paused:
        pause_started = time.monotonic_ns()
        paused = True
    elif not p and paused:
        gap += time.monotonic_ns() - pause_started
        paused = False

def on_stdin(_ch, _cond):
    line = sys.stdin.readline()
    if not line:
        send_eos()
        return False
    cmd = line.strip()
    if cmd == 'pause':
        set_paused(True)
    elif cmd == 'resume':
        set_paused(False)
    elif cmd == 'mute' and vol:
        vol.set_property('mute', True)
    elif cmd == 'unmute' and vol:
        vol.set_property('mute', False)
    elif cmd == 'stop':
        send_eos()
    return True

GLib.io_add_watch(sys.stdin, GLib.IO_IN | GLib.IO_HUP, on_stdin)
pipeline.set_state(Gst.State.PLAYING)
loop.run()
sys.exit(exit_code)
"#;

/// python3 + gi + Gst present? Cached; decides controller vs gst-launch mode.
fn controller_available() -> bool {
    use std::sync::OnceLock;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("python3")
            .args(["-c", "import gi; gi.require_version('Gst','1.0'); from gi.repository import Gst"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

fn controller_script_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| Error::Record("no config dir".into()))?
        .join("ashot");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("controller.py");
    std::fs::write(&path, CONTROLLER_PY)?;
    Ok(path)
}

#[derive(Debug, Clone)]
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
    /// Also record system audio (the default output's monitor source).
    pub system_audio: bool,
    /// Apply webrtcdsp voice processing (noise suppression/AGC) to the mic
    /// chain. Defaults to true; set false for the plain pre-processing path.
    pub voice_process: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            output: None,
            height: None,
            crop: None,
            mic: None,
            system_audio: false,
            voice_process: true,
        }
    }
}

pub struct Recording {
    child: Child,
    /// stdin of the controller subprocess (None in gst-launch fallback mode).
    control: Option<std::process::ChildStdin>,
    stop_keeper: mpsc::Sender<()>,
    pub path: PathBuf,
    pub encoder: &'static str,
    pub out_width: u32,
    pub out_height: u32,
    pub has_audio: bool,
    paused: bool,
    muted: bool,
    /// Recorded time before the current segment (pause-aware elapsed).
    accumulated: Duration,
    segment_start: Instant,
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
        .map_or(32_000, |(_, _, kbps)| *kbps);

    // Startup can die transiently (GPU format negotiation, driver quirks, and
    // a latency-negotiation race between the audio branches and the mixer in
    // dual-source mode), so retry: same encoder once, then the CPU fallback.
    let attempts: &[&'static str] = if gst_has_element("vah264enc") {
        &["vah264enc", "vah264enc", "x264enc", "x264enc"]
    } else {
        &["x264enc", "x264enc", "x264enc"]
    };
    let mut keeper = Some(keeper);
    let mut stream = stream;
    let mut last_status = String::new();
    for (i, enc) in attempts.iter().enumerate() {
        if i > 0 {
            // Portal fd was consumed by the dead child; need a fresh stream.
            drop(keeper.take());
            let (keeper2, stream2) = open_portal_stream()?;
            keeper = Some(keeper2);
            stream = stream2;
        }
        let spawn_instant = Instant::now();
        let (encoder, mut child) = spawn_gst(&stream, &opts, &path, out_w, out_h, bitrate, enc)?;
        std::thread::sleep(Duration::from_millis(1200));
        if let Ok(Some(status)) = child.try_wait() {
            last_status = status.to_string();
            eprintln!(
                "{}",
                serde_json::json!({
                    "warning": format!(
                        "{enc} pipeline exited at startup ({status}); attempt {}/{}",
                        i + 1,
                        attempts.len()
                    ),
                })
            );
            continue;
        }
        return finish_start(
            keeper.take().unwrap(),
            child,
            encoder,
            path,
            out_w,
            out_h,
            opts.mic.is_some() || opts.system_audio,
            spawn_instant,
        );
    }
    Err(Error::Record(format!("encoder exited at startup after retries: {last_status}")))
}

fn finish_start(
    keeper: mpsc::Sender<()>,
    mut child: Child,
    encoder: &'static str,
    path: PathBuf,
    out_width: u32,
    out_height: u32,
    has_audio: bool,
    started: Instant,
) -> Result<Recording> {
    let control = child.stdin.take();
    Ok(Recording {
        child,
        control,
        stop_keeper: keeper,
        path,
        encoder,
        out_width,
        out_height,
        has_audio,
        paused: false,
        muted: false,
        accumulated: Duration::ZERO,
        segment_start: started,
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
    /// Recorded time, excluding paused stretches.
    pub fn elapsed(&self) -> Duration {
        if self.paused {
            self.accumulated
        } else {
            self.accumulated + self.segment_start.elapsed()
        }
    }

    /// Pause/resume/mute are available in controller mode only.
    pub fn can_control(&self) -> bool {
        self.control.is_some()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    fn send_command(&mut self, cmd: &str) -> Result<()> {
        let Some(control) = self.control.as_mut() else {
            return Err(Error::Record("recording is not controllable".into()));
        };
        writeln!(control, "{cmd}")
            .and_then(|_| control.flush())
            .map_err(|e| Error::Record(format!("controller command failed: {e}")))
    }

    pub fn toggle_pause(&mut self) -> Result<()> {
        if self.paused {
            self.send_command("resume")?;
            self.segment_start = Instant::now();
            self.paused = false;
        } else {
            self.send_command("pause")?;
            self.accumulated += self.segment_start.elapsed();
            self.paused = true;
        }
        Ok(())
    }

    pub fn set_muted(&mut self, muted: bool) -> Result<()> {
        self.send_command(if muted { "mute" } else { "unmute" })?;
        self.muted = muted;
        Ok(())
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

/// Voice-processing fragment shared by recording and the pre-record mic
/// check (`micmon`), so what the check meters/plays is exactly what a
/// recording captures. AGC evens out quiet and hot mics (with a limiter
/// against clipping) and noise suppression cleans the floor; empty string
/// (plain chain) when disabled or webrtcdsp is missing.
pub(crate) fn mic_dsp(voice_process: bool) -> &'static str {
    if voice_process && gst_has_element("webrtcdsp") {
        "! audio/x-raw,rate=48000,channels=1,format=S16LE \
         ! webrtcdsp echo-cancel=false noise-suppression=true \
         noise-suppression-level=low gain-control=true \
         gain-control-mode=fixed-digital target-level-dbfs=9 \
         limiter=true high-pass-filter=false \
         ! audioconvert "
    } else {
        ""
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
             ! identity name=vgate \
             ! vah264enc bitrate={bitrate_kbps} ! h264parse "
        );
    } else {
        line += &format!(
            "! videoconvert ! videoscale ! video/x-raw,format=I420,width={out_w},height={out_h} \
             ! identity name=vgate \
             ! x264enc bitrate={bitrate_kbps} speed-preset=veryfast tune=zerolatency key-int-max=120 \
             ! h264parse "
        );
    }
    // Deep compressed-side queue: when the audio path runs behind (e.g. the
    // audiomixer's latency in the dual-source case), mp4mux holds video back;
    // without slack here that backpressure reaches pipewiresrc, which aborts
    // the stream. Post-encoder H.264 is cheap to buffer (~1.2MB/s at 10Mbps).
    line += "! queue max-size-time=10000000000 max-size-buffers=0 max-size-bytes=0 ! mux. ";
    // mp4mux is aggregator-based (gst >= 1.24) and runs in live mode when fed
    // by live sources; its default deadline is far shorter than the portal +
    // encoder warm-up, and a missed deadline flushes the pipeline (surfacing
    // as "Internal data stream error" from an audio source). `latency` is the
    // documented budget for exactly that.
    line += &format!("mp4mux name=mux latency=5000000000 ! filesink location={} ", shell_safe(path)?);
    let system_monitor = if opts.system_audio { default_monitor_source() } else { None };
    if opts.mic.is_some() || system_monitor.is_some() {
        if let Some(aac) = ["avenc_aac", "voaacenc"].iter().find(|e| gst_has_element(e)) {
            // Deep (10s) audio queues: mp4mux doesn't flow until the video
            // branch produces (portal/VA-API warm-up can take seconds), and a
            // default 1s queue fills, blocks pulsesrc, and drops samples —
            // audible as heavy mic stutter at the start of every recording.
            // Buffer the start-up stall instead of dropping through it.
            const AQ: &str = "queue max-size-time=10000000000 max-size-buffers=0 max-size-bytes=0";
            match (&opts.mic, &system_monitor) {
                // Both sources: they must be mixed, but audiomixer's default
                // latency of 0 silently DROPS live buffers that arrive late
                // (encoder warm-up load makes that the norm early on) and
                // zero-fills their timeslots — heavy mic stutter. A generous
                // latency lets late buffers land instead. (`vol` = mic mute
                // hook, `agate` = pause hook, both driven by the controller.)
                (Some(mic), Some(monitor)) => {
                    let device =
                        if mic == "default" { String::new() } else { format!(" device={mic}") };
                    let dsp = mic_dsp(opts.voice_process);
                    // Every mixer pad and its output carry these exact caps.
                    // With one branch's format pinned (webrtcdsp) and the
                    // other free, whichever pad negotiates first decides the
                    // mixer format and the loser's push can land mid-
                    // renegotiation and die with not-negotiated — the
                    // startup race behind "Internal data stream error".
                    // Identical pinned caps everywhere make negotiation
                    // order irrelevant.
                    const ACAPS: &str = "audio/x-raw,rate=48000,channels=1,format=S16LE";
                    line += &format!(
                        "audiomixer name=amix min-upstream-latency=700000000 ! {ACAPS} \
                         ! audioconvert ! audioresample \
                         ! identity name=agate ! {aac} bitrate=128000 ! aacparse ! {AQ} ! mux. "
                    );
                    line += &format!(
                        "pulsesrc{device} ! {AQ} ! audioconvert ! audioresample \
                         {dsp}! audioconvert ! audioresample ! {ACAPS} \
                         ! volume name=vol ! amix. "
                    );
                    line += &format!(
                        "pulsesrc device={monitor} ! {AQ} ! audioconvert ! audioresample \
                         ! {ACAPS} ! amix. "
                    );
                }
                // Single source: no mixer at all — a straight chain has no
                // output timeline to fall behind, so nothing gets dropped.
                (mic, monitor) => {
                    let (device, dsp) = match mic {
                        Some(m) => (
                            if m == "default" { String::new() } else { format!(" device={m}") },
                            mic_dsp(opts.voice_process),
                        ),
                        // Monitor-only: `vol` then mutes system audio, which
                        // is the closest thing the mute button can mean here.
                        None => (
                            format!(" device={}", monitor.as_deref().unwrap_or_default()),
                            "",
                        ),
                    };
                    line += &format!(
                        "pulsesrc{device} ! {AQ} ! audioconvert ! audioresample \
                         {dsp}! volume name=vol ! identity name=agate \
                         ! {aac} bitrate=128000 ! aacparse ! {AQ} ! mux. "
                    );
                }
            }
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

    let raw_fd = stream.fd.try_clone().map_err(|e| Error::Record(e.to_string()))?.into_raw_fd();
    let mut cmd;
    if controller_available() {
        // Controllable: python-gi runs the pipeline, commands over stdin.
        let script = controller_script_path()?;
        cmd = Command::new("python3");
        cmd.arg(script).arg(&line);
        // stderr passes through: gst-error lines land in our caller's stderr.
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::inherit());
    } else {
        let mut args: Vec<String> = vec!["-e".into(), "-q".into()];
        args.extend(split_launch_line(&line));
        cmd = Command::new("gst-launch-1.0");
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    }
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

/// The default output's monitor source (system audio), e.g.
/// `alsa_output.pci-....analog-stereo.monitor`.
pub fn default_monitor_source() -> Option<String> {
    let output = Command::new("pactl").arg("get-default-sink").output().ok()?;
    let sink = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sink.is_empty() {
        return None;
    }
    Some(format!("{sink}.monitor"))
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
