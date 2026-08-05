//! Pre-record mic check: live level metering and optional self-monitoring
//! ("hear myself") so an external mic's gain can be calibrated before a
//! recording starts. Runs the exact same pulsesrc + voice-processing chain
//! as `record::spawn_gst` (via `record::mic_dsp`), feeding a GStreamer
//! `level` element whose messages are parsed off `gst-launch-1.0 -m`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::record;

/// Below this everything reads as silence on the meter.
pub const FLOOR_DB: f32 = -60.0;

/// A running mic-check pipeline. Dropping it kills the child process.
pub struct MicMonitor {
    child: Child,
    /// Latest (rms, peak) in dBFS, written by the stdout reader thread.
    level: Arc<Mutex<(f32, f32)>>,
}

impl MicMonitor {
    /// Latest (rms, peak) in dBFS.
    pub fn level(&self) -> (f32, f32) {
        *self.level.lock().unwrap()
    }
}

impl Drop for MicMonitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start metering `device` (`"default"`/None = default source). With
/// `monitor` the processed signal is also played back so the user can hear
/// themselves — headphones recommended to avoid feedback. `voice_process`
/// selects the same DSP chain a recording would use, letting the user A/B
/// processed vs raw live.
pub fn start(device: Option<&str>, voice_process: bool, monitor: bool) -> Result<MicMonitor> {
    let device = match device {
        Some(d) if d != "default" => format!(" device={d}"),
        _ => String::new(),
    };
    let dsp = record::mic_dsp(voice_process);
    let sink = if monitor { "autoaudiosink sync=false" } else { "fakesink sync=false" };
    // Short source buffers keep the listen-to-yourself latency low; the
    // level element reports every 50ms.
    let line = format!(
        "pulsesrc{device} buffer-time=50000 latency-time=10000 \
         ! queue ! audioconvert ! audioresample \
         {dsp}! level interval=50000000 ! audioconvert ! audioresample ! {sink}"
    );
    if std::env::var_os("ASHOT_DEBUG").is_some() {
        eprintln!("[ashot debug] micmon pipeline: {line}");
    }
    let mut child = Command::new("gst-launch-1.0")
        .arg("-m")
        .args(line.split_whitespace())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Record(format!("gst-launch-1.0 not available: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Record("mic monitor: no stdout".into()))?;
    let level = Arc::new(Mutex::new((FLOOR_DB, FLOOR_DB)));
    let writer = Arc::clone(&level);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if !line.contains("level,") {
                continue;
            }
            let rms = first_db(&line, "rms=");
            let peak = first_db(&line, "peak=");
            if let (Some(rms), Some(peak)) = (rms, peak) {
                *writer.lock().unwrap() = (rms, peak);
            }
        }
        // Pipeline gone (killed or died): park the meter at the floor.
        *writer.lock().unwrap() = (FLOOR_DB, FLOOR_DB);
    });

    Ok(MicMonitor { child, level })
}

/// First channel's value from a `key=(GValueArray)< v0, v1, … >` field of a
/// serialized level message.
fn first_db(line: &str, key: &str) -> Option<f32> {
    let rest = &line[line.find(key)? + key.len()..];
    let start = rest.find('<')? + 1;
    let end = rest.find('>')?;
    rest.get(start..end)?.split(',').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_level_message_fields() {
        let line = r#"Got message #97 from element "level0" (element): level, timestamp=(guint64)1000000000, peak=(GValueArray)< -14.905813, -15.2 >, decay=(GValueArray)< -14.905813 >, rms=(GValueArray)< -18.291225 >;"#;
        assert_eq!(first_db(line, "rms="), Some(-18.291225));
        assert_eq!(first_db(line, "peak="), Some(-14.905813));
        assert_eq!(first_db(line, "missing="), None);
    }
}
