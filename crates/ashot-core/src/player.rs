//! Persistent preview player for the video editor: one long-lived
//! python-gi/GStreamer `playbin` process per open video, streaming decoded
//! RGBA frames (scaled to preview width) over stdout and taking
//! seek/play/pause commands on stdin.
//!
//! This replaces the old seek-one-frame-per-tick model (a fresh subprocess +
//! pipeline per frame): playback runs against the pipeline clock at source
//! framerate with audio on the system sink, and scrubbing is a flush-seek on
//! the already-prerolled pipeline.

use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;

use tiny_skia::Pixmap;

use crate::error::{Error, Result};

const PLAYER_PY: &str = r#"
import sys, json, threading, gi
gi.require_version('Gst', '1.0')
from gi.repository import Gst, GLib

Gst.init(None)
uri, max_w = sys.argv[1], int(sys.argv[2])

caps = 'video/x-raw,format=RGBA' + (f',width={max_w}' if max_w else '')
sink_bin = Gst.parse_bin_from_description(
    f'videoconvert ! videoscale ! {caps} '
    '! appsink name=sink sync=true max-buffers=2 drop=true emit-signals=true', True)
sink = sink_bin.get_by_name('sink')
playbin = Gst.ElementFactory.make('playbin', None)
playbin.set_property('uri', uri)
playbin.set_property('video-sink', sink_bin)

out = sys.stdout.buffer
lock = threading.Lock()
loop = GLib.MainLoop()

def push(sample):
    if sample is None:
        return
    s = sample.get_caps().get_structure(0)
    w, h = s.get_value('width'), s.get_value('height')
    buf = sample.get_buffer()
    pts = buf.pts if buf.pts != Gst.CLOCK_TIME_NONE else 0
    ok, info = buf.map(Gst.MapFlags.READ)
    if not ok:
        return
    try:
        with lock:
            out.write(b'FRM0')
            out.write(int(pts).to_bytes(8, 'little'))
            out.write(w.to_bytes(4, 'little'))
            out.write(h.to_bytes(4, 'little'))
            out.write(info.data)
            out.flush()
    except BrokenPipeError:
        GLib.idle_add(loop.quit)
    finally:
        buf.unmap(info)

def event(kind, msg=''):
    data = json.dumps({'kind': kind, 'msg': msg}).encode()
    try:
        with lock:
            out.write(b'EVT0')
            out.write(len(data).to_bytes(4, 'little'))
            out.write(data)
            out.flush()
    except BrokenPipeError:
        GLib.idle_add(loop.quit)

sink.connect('new-sample', lambda s: (push(s.emit('pull-sample')), Gst.FlowReturn.OK)[1])
sink.connect('new-preroll', lambda s: (push(s.emit('pull-preroll')), Gst.FlowReturn.OK)[1])

last_t = [0.0]

def do_seek(t):
    playbin.seek_simple(Gst.Format.TIME,
                        Gst.SeekFlags.FLUSH | Gst.SeekFlags.ACCURATE,
                        int(max(t, 0.0) * Gst.SECOND))

tried_nosound = [False]

def on_msg(bus, msg):
    if msg.type == Gst.MessageType.EOS:
        event('eos')
    elif msg.type == Gst.MessageType.ERROR:
        err, _ = msg.parse_error()
        if not tried_nosound[0]:
            # Most likely a missing/failed audio sink (headless session):
            # retry video-only rather than dying.
            tried_nosound[0] = True
            playbin.set_state(Gst.State.NULL)
            playbin.set_property('flags', int(playbin.get_property('flags')) & ~0x2)
            playbin.set_state(Gst.State.PAUSED)
            playbin.get_state(10 * Gst.SECOND)
            do_seek(last_t[0])
        else:
            event('error', err.message)

bus = playbin.get_bus()
bus.add_signal_watch()
bus.connect('message', on_msg)

def cmds():
    for line in sys.stdin:
        try:
            c = json.loads(line)
        except ValueError:
            continue
        cmd = c.get('cmd')
        if cmd == 'seek':
            last_t[0] = float(c.get('t', 0.0))
            do_seek(last_t[0])
        elif cmd == 'play':
            playbin.set_state(Gst.State.PLAYING)
        elif cmd == 'pause':
            playbin.set_state(Gst.State.PAUSED)
        elif cmd == 'quit':
            break
    GLib.idle_add(loop.quit)

threading.Thread(target=cmds, daemon=True).start()

playbin.set_state(Gst.State.PAUSED)
playbin.get_state(10 * Gst.SECOND)
do_seek(0.0)
loop.run()
playbin.set_state(Gst.State.NULL)
"#;

/// Everything the reader thread can hand back to the UI.
pub enum PlayerEvent {
    /// A decoded frame: source-time seconds + preview-scaled pixmap.
    Frame { t: f64, pixmap: Pixmap },
    Eos,
    Error(String),
}

pub struct PreviewPlayer {
    child: Child,
    stdin: ChildStdin,
}

impl PreviewPlayer {
    /// Spawn the helper for `path`, streaming frames scaled to at most
    /// `max_width` px wide (0 = native). Events are delivered on `tx` from a
    /// dedicated reader thread; the thread exits when the helper does or the
    /// receiver is dropped.
    pub fn spawn(path: &Path, max_width: u32, tx: Sender<PlayerEvent>) -> Result<Self> {
        let script = crate::video::helper_path("player.py", PLAYER_PY)?;
        let uri = format!(
            "file://{}",
            path.canonicalize().map_err(|e| Error::Record(e.to_string()))?.display()
        );
        let mut child = Command::new("python3")
            .arg(script)
            .arg(uri)
            .arg(max_width.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Record(format!("python3: {e}")))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            let mut r = BufReader::with_capacity(1 << 20, stdout);
            let mut magic = [0u8; 4];
            loop {
                if r.read_exact(&mut magic).is_err() {
                    break;
                }
                let event = match &magic {
                    b"FRM0" => {
                        let mut hdr = [0u8; 16];
                        if r.read_exact(&mut hdr).is_err() {
                            break;
                        }
                        let pts = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
                        let w = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
                        let h = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
                        let mut data = vec![0u8; (w as usize) * (h as usize) * 4];
                        if r.read_exact(&mut data).is_err() {
                            break;
                        }
                        let Some(mut pixmap) = Pixmap::new(w, h) else { break };
                        pixmap.data_mut().copy_from_slice(&data);
                        PlayerEvent::Frame { t: pts as f64 / 1e9, pixmap }
                    }
                    b"EVT0" => {
                        let mut len = [0u8; 4];
                        if r.read_exact(&mut len).is_err() {
                            break;
                        }
                        let mut data = vec![0u8; u32::from_le_bytes(len) as usize];
                        if r.read_exact(&mut data).is_err() {
                            break;
                        }
                        match serde_json::from_slice::<serde_json::Value>(&data) {
                            Ok(v) if v["kind"] == "eos" => PlayerEvent::Eos,
                            Ok(v) => PlayerEvent::Error(
                                v["msg"].as_str().unwrap_or("player error").to_string(),
                            ),
                            Err(_) => continue,
                        }
                    }
                    // Desynced stream: bail rather than misparse frames.
                    _ => break,
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, stdin })
    }

    fn send(&mut self, v: serde_json::Value) {
        let _ = writeln!(self.stdin, "{v}");
        let _ = self.stdin.flush();
    }

    /// Flush-seek to `t` seconds. While paused this prerolls (and emits) the
    /// frame at `t`; while playing, playback continues from `t`.
    pub fn seek(&mut self, t: f64) {
        self.send(serde_json::json!({ "cmd": "seek", "t": t }));
    }

    pub fn play(&mut self) {
        self.send(serde_json::json!({ "cmd": "play" }));
    }

    pub fn pause(&mut self) {
        self.send(serde_json::json!({ "cmd": "pause" }));
    }
}

impl Drop for PreviewPlayer {
    fn drop(&mut self) {
        self.send(serde_json::json!({ "cmd": "quit" }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
