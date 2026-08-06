//! AI captions: whisper.cpp via whisper-rs, English models.
//!
//! CPU inference today; the crate's `vulkan` feature flips it onto the GPU
//! once Vulkan dev headers + glslc are installed (see README).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";
const MODEL_FILE: &str = "ggml-base.en.bin";

fn model_path() -> Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| Error::Record("no cache dir".into()))?
        .join("ashot")
        .join("models");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(MODEL_FILE))
}

/// Download the model on first use (~148 MB). Returns the local path.
pub fn ensure_model(mut progress: impl FnMut(&str)) -> Result<PathBuf> {
    let path = model_path()?;
    if path.exists() {
        return Ok(path);
    }
    progress("downloading whisper model (base.en, ~148 MB)…");
    let tmp = path.with_extension("part");
    let status = Command::new("curl")
        .args(["-L", "--fail", "--silent", "--show-error", "-o"])
        .arg(&tmp)
        .arg(MODEL_URL)
        .status()
        .map_err(|e| Error::Record(format!("curl: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Record("model download failed".into()));
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Decode any input's audio to 16 kHz mono s16 wav (whisper's input format).
pub(crate) fn extract_wav(input: &Path, wav: &Path) -> Result<()> {
    let status = Command::new("gst-launch-1.0")
        .args(["-q", "filesrc"])
        .arg(format!("location={}", input.display()))
        .args(["!", "decodebin", "!", "audioconvert", "!", "audioresample", "!"])
        .arg("audio/x-raw,rate=16000,channels=1,format=S16LE")
        .args(["!", "wavenc", "!", "filesink"])
        .arg(format!("location={}", wav.display()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| Error::Record(format!("gst-launch: {e}")))?;
    if !status.success() || !wav.exists() {
        return Err(Error::Record("audio extraction failed (no audio track?)".into()));
    }
    Ok(())
}

fn fmt_srt_time(cs: i64) -> String {
    let ms = cs * 10;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms % 3_600_000) / 60_000,
        (ms % 60_000) / 1000,
        ms % 1000
    )
}

/// Transcribe `input`'s audio and write SRT to `srt_out`.
pub fn generate(input: &Path, srt_out: &Path, mut progress: impl FnMut(&str)) -> Result<()> {
    let model = ensure_model(&mut progress)?;

    progress("extracting audio…");
    let wav_path = std::env::temp_dir().join(format!("ashot-cc-{}.wav", std::process::id()));
    extract_wav(input, &wav_path)?;

    progress("loading model…");
    let reader = hound::WavReader::open(&wav_path)
        .map_err(|e| Error::Record(format!("wav read: {e}")))?;
    let samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32768.0))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Record(format!("wav samples: {e}")))?;
    let _ = std::fs::remove_file(&wav_path);

    let ctx = whisper_rs::WhisperContext::new_with_params(
        model.to_str().ok_or_else(|| Error::Record("model path".into()))?,
        whisper_rs::WhisperContextParameters::default(),
    )
    .map_err(|e| Error::Record(format!("whisper init: {e}")))?;
    let mut state =
        ctx.create_state().map_err(|e| Error::Record(format!("whisper state: {e}")))?;

    progress("transcribing…");
    let mut params = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy {
        best_of: 1,
    });
    params.set_language(Some("en"));
    params.set_print_progress(false);
    params.set_print_special(false);
    params.set_print_realtime(false);
    let threads = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4);
    params.set_n_threads(threads.min(8));

    state
        .full(params, &samples)
        .map_err(|e| Error::Record(format!("whisper: {e}")))?;

    let n = state
        .full_n_segments()
        .map_err(|e| Error::Record(format!("whisper segments: {e}")))?;
    let mut srt = String::new();
    for i in 0..n {
        let text = state
            .full_get_segment_text(i)
            .map_err(|e| Error::Record(format!("segment text: {e}")))?;
        let t0 = state.full_get_segment_t0(i).unwrap_or(0);
        let t1 = state.full_get_segment_t1(i).unwrap_or(t0 + 100);
        srt.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            fmt_srt_time(t0),
            fmt_srt_time(t1),
            text.trim()
        ));
    }
    std::fs::write(srt_out, srt)?;
    progress("done");
    Ok(())
}
