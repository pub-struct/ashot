//! Waveform peak extraction for the audio lane. Reuses the exact WAV
//! extraction command `captions::extract_wav` already uses for whisper input,
//! and the same `hound` decode-to-samples step — no new dependency.

use std::path::Path;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PeakData {
    /// Empty if the source has no audio track or extraction failed.
    pub peaks: Vec<(f32, f32)>, // (min, max) per bucket, values in [-1.0, 1.0]
    pub duration_s: f64,
}

/// Compute ~`target_buckets` (clamped to 1000..=2000) min/max sample pairs
/// across the whole track, for a waveform lane. Never errors on "no audio" or
/// decode failure — returns `PeakData::default()` (empty `peaks`) instead, so
/// the waveform lane simply renders empty rather than surfacing a UI error.
pub fn extract_peaks(path: &Path, target_buckets: usize) -> PeakData {
    let wav_path = std::env::temp_dir().join(format!("ashot-peaks-{}.wav", std::process::id()));
    if crate::captions::extract_wav(path, &wav_path).is_err() {
        return PeakData::default();
    }
    let result = (|| -> crate::error::Result<PeakData> {
        let reader = hound::WavReader::open(&wav_path)
            .map_err(|e| crate::error::Error::Record(format!("wav read: {e}")))?;
        let sample_rate = reader.spec().sample_rate.max(1) as f64;
        let samples: Vec<i16> = reader
            .into_samples::<i16>()
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| crate::error::Error::Record(format!("wav samples: {e}")))?;
        let duration_s = samples.len() as f64 / sample_rate;
        let buckets = target_buckets.clamp(1000, 2000);
        let bucket_size = (samples.len() / buckets).max(1);
        let mut peaks = Vec::with_capacity(buckets);
        for chunk in samples.chunks(bucket_size) {
            let mut lo = 0f32;
            let mut hi = 0f32;
            for &s in chunk {
                let v = s as f32 / 32768.0;
                if v < lo {
                    lo = v;
                }
                if v > hi {
                    hi = v;
                }
            }
            peaks.push((lo, hi));
        }
        Ok(PeakData { peaks, duration_s })
    })();
    let _ = std::fs::remove_file(&wav_path);
    result.unwrap_or_default()
}
