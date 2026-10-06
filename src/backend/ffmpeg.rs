//! ffmpeg helpers shared by the recording backends and the converter.
//!
//! Two things the call sites kept getting wrong:
//!
//! * **Encoder availability.** The native backend pipes raw frames into
//!   ffmpeg, so an unknown encoder only surfaces after the whole recording
//!   is spent. Probing up front turns that into an instant, actionable
//!   error.
//! * **Swallowed stderr.** With `stderr(Stdio::null())` a failed ffmpeg is
//!   just `exit status: 1`. Redirecting it to a file keeps the real message
//!   available for the error report.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;

/// A video encoder that is present in this ffmpeg build, with the flags it
/// needs for raw RGBA input.
#[derive(Debug)]
pub struct VideoEncoder {
    pub name: &'static str,
    pub flags: &'static [&'static str],
}

/// Parse the encoder names out of `ffmpeg -encoders` output.
fn parse_encoders(text: &str) -> HashSet<String> {
    text.lines()
        // Encoder lines look like ` V....D libx264rgb   libx264 ... (codec h264)`;
        // the legend (` V..... = Video`) is dropped by the `=` check.
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            fields.next()?;
            let name = fields.next()?;
            (name != "=").then(|| name.to_owned())
        })
        .collect()
}

/// Encoder names reported by `ffmpeg -encoders`, probed once per process
/// (`detect()` and `record()` both ask).
pub fn available_encoders() -> Result<HashSet<String>> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    if let Some(cached) = CACHE.get() {
        return Ok(cached.clone());
    }

    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-encoders"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run ffmpeg — is it installed?")?;

    if !out.status.success() {
        bail!(
            "ffmpeg --encoders failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let encoders = parse_encoders(&String::from_utf8_lossy(&out.stdout));
    let _ = CACHE.set(encoders.clone());
    Ok(encoders)
}

/// Pick the best encoder for lossless-ish RGB capture.
///
/// `libx264rgb` keeps the compositor's pixels exact, which is what the
/// palette pass in [`crate::converter`] wants; the fallbacks trade a little
/// quality for availability.
pub fn pick_video_encoder() -> Result<VideoEncoder> {
    choose_encoder(&available_encoders()?)
}

/// First usable encoder from the preference chain, given what ffmpeg has.
fn choose_encoder(available: &HashSet<String>) -> Result<VideoEncoder> {
    for (name, flags) in [
        ("libx264rgb", &["-crf", "18"][..]),
        ("libx264", &["-pix_fmt", "yuv420p", "-crf", "18"]),
        ("mpeg4", &["-q:v", "3"]),
    ] {
        if available.contains(name) {
            return Ok(VideoEncoder { name, flags });
        }
    }
    bail!(
        "ffmpeg has no usable video encoder (tried libx264rgb, libx264, mpeg4)\n  \
         install an ffmpeg build with libx264 enabled"
    )
}

/// Base ffmpeg command: no banner, no progress chatter, overwrite allowed.
pub fn ffmpeg() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Open `path` as an ffmpeg stderr target.
pub fn log_file(path: &Path) -> Result<File> {
    File::create(path).with_context(|| format!("failed to open log file {}", path.display()))
}

/// The last few non-empty lines of an ffmpeg log, indented for an error.
pub fn log_tail(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .collect();
    lines
        .iter()
        .rev()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Error describing a failed ffmpeg run, including its log tail.
pub fn failed(status: &ExitStatus, log: &Path) -> anyhow::Error {
    let tail = log_tail(log);
    let tail = if tail.is_empty() {
        String::new()
    } else {
        format!("\n{tail}")
    };
    anyhow!("ffmpeg exited with {status}{tail}")
}

/// Context for a frame write that failed: ffmpeg is gone, so its log is the
/// only explanation available.
pub fn write_failed(log: &Path) -> String {
    let tail = log_tail(log);
    if tail.is_empty() {
        "failed to write frame to ffmpeg (it exited)".to_owned()
    } else {
        format!("failed to write frame to ffmpeg; it reported:\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Encoders:\n V..... = Video\n A..... = Audio\n ------\n\
         V....D a64multi             Multicolor charset for Commodore 64 (codec a64_multi)\n\
         V....D libx264              libx264 H.264 (codec h264)\n\
         V....D libx264rgb           libx264 H.264 RGB (codec h264)\n\
         V..... rawvideo             raw video (codec rawvideo)\n";

    #[test]
    fn parses_encoder_names_and_skips_the_legend() {
        let encoders = parse_encoders(SAMPLE);
        assert!(encoders.contains("libx264rgb"));
        assert!(encoders.contains("rawvideo"));
        assert!(!encoders.contains("="));
        assert!(!encoders.contains("Video"));
        assert!(!encoders.contains("Encoders:"));
        assert_eq!(encoders.len(), 4);
    }

    #[test]
    fn prefers_lossless_rgb_then_falls_back() {
        let all = parse_encoders(SAMPLE);
        assert_eq!(choose_encoder(&all).unwrap().name, "libx264rgb");

        let no_rgb: HashSet<String> = ["libx264", "rawvideo"].map(str::to_owned).into();
        let encoder = choose_encoder(&no_rgb).unwrap();
        assert_eq!(encoder.name, "libx264");
        assert_eq!(encoder.flags, ["-pix_fmt", "yuv420p", "-crf", "18"]);

        let only_mpeg4: HashSet<String> = ["mpeg4"].map(str::to_owned).into();
        assert_eq!(choose_encoder(&only_mpeg4).unwrap().name, "mpeg4");

        let none: HashSet<String> = ["rawvideo"].map(str::to_owned).into();
        let err = choose_encoder(&none).unwrap_err().to_string();
        assert!(err.contains("no usable video encoder"), "{err}");
    }

    #[test]
    fn encoder_names_survive_real_ffmpeg_output() {
        // A real probe is the only check that the parser matches the format.
        let Ok(encoders) = available_encoders() else {
            eprintln!("skipped: ffmpeg is not installed");
            return;
        };
        assert!(encoders.len() > 10, "{encoders:?}");
    }
}
