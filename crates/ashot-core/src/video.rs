//! Video editing core: probe, frame extraction, and the GPU export pipeline
//! (cuts, animated zoom, drawing overlay, caption burn).
//!
//! Processing runs in embedded python-gi helpers (same approach as the
//! recorder controller): decode on the GPU (`vah264dec`), animated zoom as
//! per-frame crop metadata applied by `vapostproc` on the GPU, re-encode with
//! `vah264enc`. Cuts are per-segment transcodes joined losslessly by gst's
//! `concat` element in a final remux.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tiny_skia::Pixmap;

use crate::error::{Error, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct VideoInfo {
    pub duration_s: f64,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
}

/// A smooth zoom event: eases in around `t`, holds, eases out.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ZoomPoint {
    /// Center time in seconds.
    pub t: f64,
    /// Zoom center in source pixels.
    pub cx: f64,
    pub cy: f64,
    /// Magnification (2.0 = 2×).
    pub level: f64,
    /// Total length of the zoomed stretch (ease + hold + ease).
    pub duration: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ExportSpec {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Ranges to KEEP, in seconds, ordered. Empty = whole video.
    pub keep: Vec<(f64, f64)>,
    pub zooms: Vec<ZoomPoint>,
    /// RGBA PNG at source resolution, composited over every frame.
    pub overlay_png: Option<PathBuf>,
    /// SRT to burn in.
    pub srt: Option<PathBuf>,
}

const FRAME_PY: &str = r#"
import sys, gi
gi.require_version('Gst', '1.0')
from gi.repository import Gst

Gst.init(None)
uri, t_ns = sys.argv[1], int(sys.argv[2])
pipeline = Gst.parse_launch(
    f'uridecodebin uri={uri} ! videoconvert ! video/x-raw,format=RGBA ! appsink name=sink sync=false')
sink = pipeline.get_by_name('sink')
pipeline.set_state(Gst.State.PAUSED)
pipeline.get_state(10 * Gst.SECOND)
pipeline.seek_simple(Gst.Format.TIME,
                     Gst.SeekFlags.FLUSH | Gst.SeekFlags.ACCURATE, t_ns)
pipeline.get_state(10 * Gst.SECOND)
sample = sink.emit('pull-preroll')
if sample is None:
    sys.exit(3)
caps = sample.get_caps().get_structure(0)
w, h = caps.get_value('width'), caps.get_value('height')
buf = sample.get_buffer()
ok, info = buf.map(Gst.MapFlags.READ)
sys.stdout.buffer.write(w.to_bytes(4, 'little'))
sys.stdout.buffer.write(h.to_bytes(4, 'little'))
sys.stdout.buffer.write(info.data)
buf.unmap(info)
pipeline.set_state(Gst.State.NULL)
"#;

const EXPORT_PY: &str = r#"
import sys, json, gi
gi.require_version('Gst', '1.0')
from gi.repository import Gst, GLib

Gst.init(None)
spec = json.load(open(sys.argv[1]))
W, H = spec['width'], spec['height']
DUR = spec['duration']
keep = spec['keep'] or [[0.0, DUR + 1.0]]
keep.sort()
EASE = 0.6

# removed_before[i] = seconds cut out before keep[i] starts.
removed = []
acc, prev_end = 0.0, 0.0
for s0, e0 in keep:
    acc += max(0.0, s0 - prev_end)
    removed.append(acc)
    prev_end = e0

def keep_map(t):
    # -> None (dropped) or the compressed timestamp.
    for (s0, e0), rem in zip(keep, removed):
        if s0 <= t < e0:
            return t - rem
    return None

def zoom_at(t):
    best = (1.0, W / 2, H / 2)
    for z in spec['zooms']:
        half = z['duration'] / 2.0
        d = abs(t - z['t'])
        if d > half:
            continue
        k = min(1.0, (half - d) / EASE)
        k = k * k * (3 - 2 * k)
        best = (1.0 + (z['level'] - 1.0) * k, z['cx'], z['cy'])
    return best

def crop_for(t):
    level, cx, cy = zoom_at(t)
    if level <= 1.001:
        return (0, 0, 0, 0)
    vw, vh = W / level, H / level
    x0 = min(max(cx - vw / 2, 0), W - vw)
    y0 = min(max(cy - vh / 2, 0), H - vh)
    even = lambda v: max(0, int(v) & ~1)
    return (even(x0), even(W - (x0 + vw)), even(y0), even(H - (y0 + vh)))

inp, out = spec['input'], spec['output']
overlay, srt = spec.get('overlay'), spec.get('srt')
bitrate = spec['bitrate']

post = ''
if overlay or srt:
    # Let videoconvert negotiate whatever the overlay elements support,
    # then hand the encoder NV12 explicitly.
    post += '! videoconvert '
    if overlay:
        post += f'! gdkpixbufoverlay location="{overlay}" overlay-width={W} overlay-height={H} '
    if srt:
        post += '! subtitleoverlay name=subover font-desc="Sans, 24" '
    post += '! videoconvert ! video/x-raw,format=NV12 '

if spec['gpu']:
    decode = '! h264parse ! vah264dec '
    if overlay or srt:
        # CPU compositing ahead: vapostproc downloads to system memory here.
        scale = f'! vapostproc ! video/x-raw,width={W},height={H} '
    else:
        scale = f'! vapostproc ! video/x-raw(memory:VAMemory),format=NV12,width={W},height={H} '
    encode = f'! vah264enc bitrate={bitrate} ! h264parse '
else:
    decode = '! h264parse ! avdec_h264 '
    scale = f'! videoscale ! video/x-raw,width={W},height={H} '
    encode = f'! x264enc bitrate={bitrate} speed-preset=veryfast ! h264parse '

desc = (f'filesrc location="{inp}" ! qtdemux name=d '
        f'd.video_0 ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=100000000 '
        f'{decode}! videocrop name=zc {scale}{post}{encode}'
        f'! queue ! mp4mux name=m ! filesink location="{out}" ')
if srt:
    desc += f'filesrc location="{srt}" ! subparse ! subover.subtitle_sink '
if spec['has_audio']:
    desc += ('d.audio_0 ! queue max-size-buffers=0 max-size-time=0 max-size-bytes=100000000 '
             '! aacparse ! avdec_aac ! audioconvert ! audioresample '
             '! identity name=agate ! avenc_aac bitrate=128000 ! aacparse ! queue ! m. ')

pipeline = Gst.parse_launch(desc)
last_progress = [0.0]

def video_probe(_pad, info):
    buf = info.get_buffer()
    if buf is None or buf.pts == Gst.CLOCK_TIME_NONE:
        return Gst.PadProbeReturn.OK
    t = buf.pts / Gst.SECOND
    mapped = keep_map(t)
    if mapped is None:
        return Gst.PadProbeReturn.DROP
    l, r, tp, b = crop_for(t)
    zc.set_property('left', l)
    zc.set_property('right', r)
    zc.set_property('top', tp)
    zc.set_property('bottom', b)
    buf.pts = int(mapped * Gst.SECOND)
    if t - last_progress[0] > 1.0:
        last_progress[0] = t
        print(json.dumps({'progress': min(0.99, t / max(DUR, 0.01))}), flush=True)
    return Gst.PadProbeReturn.OK

def audio_probe(_pad, info):
    buf = info.get_buffer()
    if buf is None or buf.pts == Gst.CLOCK_TIME_NONE:
        return Gst.PadProbeReturn.OK
    mapped = keep_map(buf.pts / Gst.SECOND)
    if mapped is None:
        return Gst.PadProbeReturn.DROP
    buf.pts = int(mapped * Gst.SECOND)
    return Gst.PadProbeReturn.OK

zc = pipeline.get_by_name('zc')
zc.get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER, video_probe)
agate = pipeline.get_by_name('agate')
if agate:
    agate.get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, audio_probe)

loop = GLib.MainLoop()
code = [0]

def on_msg(bus, msg):
    if msg.type == Gst.MessageType.EOS:
        loop.quit()
    elif msg.type == Gst.MessageType.ERROR:
        err, _ = msg.parse_error()
        print('gst-error: ' + err.message, file=sys.stderr, flush=True)
        code[0] = 1
        loop.quit()

bus = pipeline.get_bus()
bus.add_signal_watch()
bus.connect('message', on_msg)
pipeline.set_state(Gst.State.PLAYING)
loop.run()
pipeline.set_state(Gst.State.NULL)
print(json.dumps({'progress': 1.0}), flush=True)
sys.exit(code[0])
"#;

fn helper_path(name: &str, source: &str) -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| Error::Record("no config dir".into()))?
        .join("ashot");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, source)?;
    Ok(path)
}

/// Duration/dimensions/audio via gst-discoverer.
pub fn probe(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("gst-discoverer-1.0")
        .arg("-v")
        .arg(path)
        .output()
        .map_err(|e| Error::Record(format!("gst-discoverer-1.0: {e}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut duration_s = 0.0;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut has_audio = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Duration: ") {
            // 0:00:08.136533333
            let parts: Vec<&str> = rest.trim().split(':').collect();
            if parts.len() == 3 {
                let h: f64 = parts[0].parse().unwrap_or(0.0);
                let m: f64 = parts[1].parse().unwrap_or(0.0);
                let s: f64 = parts[2].parse().unwrap_or(0.0);
                duration_s = h * 3600.0 + m * 60.0 + s;
            }
        } else if line.starts_with("Width:") {
            width = line.split(':').nth(1).unwrap_or("0").trim().parse().unwrap_or(0);
        } else if line.starts_with("Height:") {
            height = line.split(':').nth(1).unwrap_or("0").trim().parse().unwrap_or(0);
        } else if line.contains("audio #") || line.starts_with("Audio:") {
            has_audio = true;
        }
    }
    if duration_s <= 0.0 || width == 0 {
        return Err(Error::Record(format!("could not probe {}", path.display())));
    }
    Ok(VideoInfo { duration_s, width, height, has_audio })
}

/// Decode the frame at `t` seconds as a Pixmap (native resolution).
pub fn extract_frame(path: &Path, t: f64) -> Result<Pixmap> {
    let script = helper_path("frame.py", FRAME_PY)?;
    let uri = format!(
        "file://{}",
        path.canonicalize().map_err(|e| Error::Record(e.to_string()))?.display()
    );
    let mut child = Command::new("python3")
        .arg(script)
        .arg(uri)
        .arg(((t.max(0.0) * 1e9) as u64).to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Record(format!("python3: {e}")))?;
    let mut data = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut data)?;
    let status = child.wait()?;
    if !status.success() || data.len() < 8 {
        return Err(Error::Record("frame extraction failed".into()));
    }
    let w = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let expected = (w * h * 4) as usize;
    if data.len() < 8 + expected {
        return Err(Error::Record("short frame data".into()));
    }
    let mut pixmap =
        Pixmap::new(w, h).ok_or_else(|| Error::Record("bad frame dimensions".into()))?;
    pixmap.data_mut().copy_from_slice(&data[8..8 + expected]);
    Ok(pixmap)
}

/// Run the export; calls `progress` with 0..=1 as segments complete.
pub fn export(spec: &ExportSpec, mut progress: impl FnMut(f64)) -> Result<PathBuf> {
    let info = probe(&spec.input)?;
    let gpu = crate::record::gst_has_element("vah264dec")
        && crate::record::gst_has_element("vah264enc");
    let bitrate = crate::record::RESOLUTIONS
        .iter()
        .find(|(_, h, _)| *h >= info.height.min(1440))
        .map_or(16_000, |(_, _, kbps)| *kbps);

    let tmpdir = std::env::temp_dir().join(format!("ashot-export-{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir)?;

    let json_spec = serde_json::json!({
        "input": spec.input.canonicalize().map_err(|e| Error::Record(e.to_string()))?,
        "output": spec.output,
        "width": info.width,
        "height": info.height,
        "duration": info.duration_s,
        "has_audio": info.has_audio,
        "keep": spec.keep,
        "zooms": spec.zooms,
        "overlay": spec.overlay_png,
        "srt": spec.srt,
        "gpu": gpu,
        "bitrate": bitrate,
        "tmpdir": tmpdir,
    });
    let spec_path = tmpdir.join("spec.json");
    std::fs::write(&spec_path, serde_json::to_vec(&json_spec)?)?;

    let script = helper_path("export.py", EXPORT_PY)?;
    let mut child = Command::new("python3")
        .arg(script)
        .arg(&spec_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Record(format!("python3: {e}")))?;

    // Stream progress lines.
    if let Some(stdout) = child.stdout.take() {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(p) = v.get("progress").and_then(|p| p.as_f64()) {
                    progress(p);
                }
            }
        }
    }
    let output = child.wait_with_output()?;
    let _ = std::fs::remove_dir_all(&tmpdir);
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Record(format!(
            "export failed: {}",
            err.lines().last().unwrap_or("unknown error")
        )));
    }
    if !spec.output.exists() {
        return Err(Error::Record("export produced no file".into()));
    }
    Ok(spec.output.clone())
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Record(format!("json: {e}"))
    }
}
