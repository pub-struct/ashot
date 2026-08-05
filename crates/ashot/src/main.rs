//! ashot — agentic screenshot tool.
//!
//! Contract: success metadata as JSON on stdout, structured errors as JSON on
//! stderr (exit 1). `-o -` streams PNG bytes to stdout instead of metadata.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ashot_core::{capture, parse_spec, Annotation, Renderer};
use clap::{Parser, Subcommand};
use serde_json::json;
use tiny_skia::Pixmap;

#[derive(Parser)]
#[command(
    name = "ashot",
    version,
    about = "Agentic screenshot tool: capture and annotate, headlessly.",
    after_help = "Annotation spec (JSON, coordinates in image pixels):\n  \
    [{\"type\":\"rect\",\"x\":10,\"y\":10,\"w\":200,\"h\":100,\"color\":\"red\",\"label\":\"here\"},\n   \
    {\"type\":\"ellipse\",\"x\":50,\"y\":50,\"w\":80,\"h\":40},\n   \
    {\"type\":\"arrow\",\"from\":[400,300],\"to\":[250,150]},\n   \
    {\"type\":\"marker\",\"x\":120,\"y\":80},\n   \
    {\"type\":\"text\",\"x\":10,\"y\":220,\"text\":\"note\",\"size\":28}]\n\
    Pass specs inline, as @file.json, or as - for stdin."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture the screen (full desktop by default).
    Capture {
        /// Output path; `-` streams PNG to stdout. Default: ~/Pictures/Screenshots/shot-<ts>.png
        #[arg(short, long)]
        output: Option<String>,
        /// Crop to a region, in image pixels: X,Y,WxH or X,Y,W,H
        #[arg(long, value_name = "X,Y,W,H", conflicts_with = "monitor")]
        region: Option<String>,
        /// Crop to one monitor (index from `ashot monitors`).
        #[arg(long, value_name = "N")]
        monitor: Option<usize>,
        /// Apply an annotation spec to the capture (inline JSON, @file, or -).
        #[arg(long, value_name = "SPEC")]
        annotate: Option<String>,
        /// Also copy the PNG to the clipboard.
        #[arg(long)]
        clipboard: bool,
        /// Write the annotation spec next to the output as <out>.shot.json
        #[arg(long, requires = "annotate")]
        keep_spec: bool,
    },
    /// Annotate an existing image (never mutates the input).
    Annotate {
        /// Input PNG.
        input: PathBuf,
        /// Annotation spec: inline JSON, @file, or - for stdin.
        #[arg(long, value_name = "SPEC")]
        spec: String,
        /// Output path; `-` streams PNG to stdout. Default: <input>-annotated.png
        #[arg(short, long)]
        output: Option<String>,
        /// Also copy the PNG to the clipboard.
        #[arg(long)]
        clipboard: bool,
        /// Write the annotation spec next to the output as <out>.shot.json
        #[arg(long)]
        keep_spec: bool,
    },
    /// Record the screen to MP4 (GPU-encoded via VA-API when available).
    Record {
        /// Output path. Default: ~/Videos/Screencasts/rec-<ts>.mp4
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Target height: 720, 1080 or 1440. Default: native.
        #[arg(long, value_name = "720|1080|1440")]
        resolution: Option<u32>,
        /// Stop automatically after this many seconds (otherwise Ctrl+C).
        #[arg(long, value_name = "SECS")]
        duration: Option<u64>,
        /// Crop region in stream pixels: X,Y,W,H
        #[arg(long, value_name = "X,Y,W,H")]
        region: Option<String>,
        /// Record microphone audio: bare --mic = default input, or a source
        /// name from `pactl list sources short`.
        #[arg(long, value_name = "DEVICE", num_args = 0..=1, default_missing_value = "default")]
        mic: Option<String>,
    },
    /// One-time interactive grant of the screenshot permission (run as a human).
    Setup,
    /// List monitors as JSON (pick indices for `capture --monitor`).
    Monitors,
    /// Launch the desktop app: region-select overlay, or the editor on a file.
    Ui {
        /// Open this PNG directly in the annotation editor.
        file: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Capture { output, region, monitor, annotate, clipboard, keep_spec } => {
            cmd_capture(output, region, monitor, annotate, clipboard, keep_spec)
        }
        Command::Annotate { input, spec, output, clipboard, keep_spec } => {
            cmd_annotate(input, spec, output, clipboard, keep_spec)
        }
        Command::Record { output, resolution, duration, region, mic } => {
            cmd_record(output, resolution, duration, region, mic)
        }
        Command::Setup => cmd_setup(),
        Command::Monitors => cmd_monitors(),
        Command::Ui { file } => ashot_app::run(match file {
            Some(path) => ashot_app::Mode::Editor(path),
            None => ashot_app::Mode::Launcher,
        }),
    };
    if let Err(err) = result {
        let code = err
            .downcast_ref::<ashot_core::Error>()
            .map(|e| e.code())
            .unwrap_or("internal");
        eprintln!(
            "{}",
            json!({ "ok": false, "error": { "code": code, "message": err.to_string() } })
        );
        std::process::exit(1);
    }
}

fn cmd_capture(
    output: Option<String>,
    region: Option<String>,
    monitor: Option<usize>,
    annotate: Option<String>,
    clipboard: bool,
    keep_spec: bool,
) -> anyhow::Result<()> {
    let captured = if let Some(region) = region {
        let (x, y, w, h) = parse_region(&region)?;
        capture::capture_region(x, y, w, h)?
    } else if let Some(index) = monitor {
        capture::capture_monitor(index)?
    } else {
        capture::capture_full()?
    };

    let mut pixmap = captured.pixmap;
    let annotations = match annotate {
        Some(spec_arg) => {
            let spec = parse_spec(&read_spec_arg(&spec_arg)?)?;
            Renderer::new().render(&mut pixmap, &spec)?;
            spec
        }
        None => Vec::new(),
    };

    let dest = resolve_output(output, || default_capture_path())?;
    let written = write_output(&pixmap, &dest, clipboard)?;
    if keep_spec {
        write_sidecar(&written, &annotations)?;
    }

    emit(json!({
        "ok": true,
        "path": written.as_ref().map(|p| p.display().to_string()),
        "width": pixmap.width(),
        "height": pixmap.height(),
        "scale_factor": captured.scale_factor,
        "region": captured.region.map(|(x, y, w, h)| json!([x, y, w, h])),
        "annotations": annotations.len(),
        "clipboard": clipboard,
    }), &dest);
    Ok(())
}

fn cmd_annotate(
    input: PathBuf,
    spec_arg: String,
    output: Option<String>,
    clipboard: bool,
    keep_spec: bool,
) -> anyhow::Result<()> {
    let bytes = std::fs::read(&input)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", input.display()))?;
    let mut pixmap =
        Pixmap::decode_png(&bytes).map_err(|e| ashot_core::Error::Image(e.to_string()))?;

    let spec = parse_spec(&read_spec_arg(&spec_arg)?)?;
    Renderer::new().render(&mut pixmap, &spec)?;

    let dest = resolve_output(output, || default_annotated_path(&input))?;
    let written = write_output(&pixmap, &dest, clipboard)?;
    if keep_spec {
        write_sidecar(&written, &spec)?;
    }

    emit(json!({
        "ok": true,
        "path": written.as_ref().map(|p| p.display().to_string()),
        "input": input.display().to_string(),
        "width": pixmap.width(),
        "height": pixmap.height(),
        "annotations": spec.len(),
        "clipboard": clipboard,
    }), &dest);
    Ok(())
}

static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_sigint(_: i32) {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn cmd_record(
    output: Option<PathBuf>,
    resolution: Option<u32>,
    duration: Option<u64>,
    region: Option<String>,
    mic: Option<String>,
) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;

    if let Some(res) = resolution {
        anyhow::ensure!(
            [720, 1080, 1440].contains(&res),
            "resolution must be 720, 1080 or 1440"
        );
    }
    let crop = match region {
        Some(r) => {
            let (x, y, w, h) = parse_region(&r)?;
            anyhow::ensure!(x >= 0 && y >= 0, "region origin must be non-negative");
            Some((x as u32, y as u32, w, h))
        }
        None => None,
    };

    let recording = ashot_record_start(ashot_core_opts(output, resolution, crop, mic))?;
    eprintln!(
        "{}",
        json!({
            "recording": true,
            "encoder": recording.encoder,
            "width": recording.out_width,
            "height": recording.out_height,
            "path": recording.path.display().to_string(),
            "stop": if duration.is_some() { "automatic" } else { "Ctrl+C" },
        })
    );

    unsafe {
        libc::signal(libc::SIGINT, on_sigint as usize);
    }
    let mut recording = recording;
    let deadline = duration.map(|d| std::time::Instant::now() + std::time::Duration::from_secs(d));
    loop {
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            break;
        }
        if let Some(deadline) = deadline {
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        if !recording.is_running() {
            anyhow::bail!("encoder process exited unexpectedly");
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }

    let elapsed = recording.elapsed().as_secs_f64();
    let encoder = recording.encoder;
    let (w, h) = (recording.out_width, recording.out_height);
    let path = recording.stop()?;
    println!(
        "{}",
        json!({
            "ok": true,
            "action": "record",
            "path": path.display().to_string(),
            "encoder": encoder,
            "width": w,
            "height": h,
            "duration_s": (elapsed * 10.0).round() / 10.0,
        })
    );
    Ok(())
}

fn ashot_core_opts(
    output: Option<PathBuf>,
    resolution: Option<u32>,
    crop: Option<(u32, u32, u32, u32)>,
    mic: Option<String>,
) -> ashot_core::record::RecordOptions {
    ashot_core::record::RecordOptions { output, height: resolution, crop, mic }
}

fn ashot_record_start(
    opts: ashot_core::record::RecordOptions,
) -> anyhow::Result<ashot_core::record::Recording> {
    Ok(ashot_core::record::start_recording(opts)?)
}

fn cmd_setup() -> anyhow::Result<()> {
    eprintln!("Requesting screenshot permission from the desktop portal…");
    eprintln!("If a dialog appears, approve it — the grant persists and agent captures run silently afterward.");
    let captured = capture::capture_full()?;
    println!(
        "{}",
        json!({
            "ok": true,
            "message": "Screenshot permission granted; `ashot capture` will now work unattended.",
            "width": captured.pixmap.width(),
            "height": captured.pixmap.height(),
        })
    );
    Ok(())
}

fn cmd_monitors() -> anyhow::Result<()> {
    let monitors = capture::monitors()?;
    println!("{}", json!({ "ok": true, "monitors": monitors }));
    Ok(())
}

// --- plumbing ---

enum Dest {
    File(PathBuf),
    Stdout,
}

fn resolve_output(
    output: Option<String>,
    default: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<Dest> {
    match output.as_deref() {
        Some("-") => Ok(Dest::Stdout),
        Some(path) => Ok(Dest::File(PathBuf::from(path))),
        None => Ok(Dest::File(default()?)),
    }
}

fn default_capture_path() -> anyhow::Result<PathBuf> {
    Ok(ashot_core::paths::default_capture_path()?)
}

fn default_annotated_path(input: &Path) -> anyhow::Result<PathBuf> {
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    Ok(input.with_file_name(format!("{stem}-annotated.png")))
}

/// Encode and deliver the PNG. Returns the written path (None for stdout).
fn write_output(pixmap: &Pixmap, dest: &Dest, clipboard: bool) -> anyhow::Result<Option<PathBuf>> {
    let png = pixmap
        .encode_png()
        .map_err(|e| ashot_core::Error::Image(e.to_string()))?;
    let path = match dest {
        Dest::Stdout => {
            std::io::stdout().write_all(&png)?;
            None
        }
        Dest::File(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(path, &png)?;
            Some(path.clone())
        }
    };
    if clipboard {
        copy_to_clipboard(png)?;
    }
    Ok(path)
}

fn copy_to_clipboard(png: Vec<u8>) -> anyhow::Result<()> {
    Ok(ashot_core::clipboard::copy_png(png)?)
}

fn write_sidecar(written: &Option<PathBuf>, annotations: &[Annotation]) -> anyhow::Result<()> {
    let Some(path) = written else { return Ok(()) };
    let sidecar = PathBuf::from(format!("{}.shot.json", path.display()));
    let doc = json!({ "version": 1, "annotations": annotations });
    std::fs::write(&sidecar, serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

/// Metadata goes to stdout unless the PNG itself is on stdout (then stderr).
fn emit(meta: serde_json::Value, dest: &Dest) {
    match dest {
        Dest::Stdout => eprintln!("{meta}"),
        Dest::File(_) => println!("{meta}"),
    }
}

fn read_spec_arg(arg: &str) -> anyhow::Result<String> {
    if arg == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else if let Some(path) = arg.strip_prefix('@') {
        Ok(std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read spec file {path}: {e}"))?)
    } else {
        Ok(arg.to_string())
    }
}

/// Accepts "X,Y,WxH" or "X,Y,W,H".
fn parse_region(s: &str) -> anyhow::Result<(i32, i32, u32, u32)> {
    let normalized = s.replace('x', ",");
    let parts: Vec<&str> = normalized.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        anyhow::bail!("invalid region {s:?}; expected X,Y,W,H");
    }
    Ok((
        parts[0].parse()?,
        parts[1].parse()?,
        parts[2].parse()?,
        parts[3].parse()?,
    ))
}
