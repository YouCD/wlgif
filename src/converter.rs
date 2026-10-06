use crate::backend::ffmpeg;
use crate::error::Error;
use crate::output;
use anyhow::{Context, Result};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Convert a video file to an optimized GIF.
pub fn to_gif(
    input: &Path,
    output: &Path,
    fps: u32,
    width: Option<u32>,
    optimize: bool,
    quiet: bool,
    progress: Option<&mut dyn FnMut(f64)>,
) -> Result<()> {
    if !quiet {
        eprintln!();
        output::status("Converting to GIF");
    }

    let scale = match width {
        Some(w) => format!("scale={}:-1:flags=lanczos", w),
        None => "scale=trunc(iw/2)*2:trunc(ih/2)*2".into(),
    };

    let base_filter = format!("fps={},{}", fps, scale);
    let total_frames = probe_total_frames(input, fps).unwrap_or(0);
    let mut noop = |_p: f64| {};
    let progress: &mut dyn FnMut(f64) = progress.unwrap_or(&mut noop);

    if optimize {
        convert_optimized(input, output, &base_filter, quiet, total_frames, progress)?;
    } else {
        convert_fast(input, output, &base_filter, total_frames, progress)?;
    }

    Ok(())
}

/// Re-encode a captured video into a broadly playable H.264 file.
///
/// The native backend records with `libx264rgb` so the palette pass sees the
/// compositor's exact pixels; the price is an MP4 carrying planar RGB
/// (`gbrp`), which browsers and hardware decoders refuse. A video the user
/// asked to keep goes through `libx264`/`yuv420p` instead. Without that
/// encoder the original file is copied unchanged — a lossless file that
/// plays nowhere beats no file at all.
pub fn to_h264_video(input: &Path, output: &Path, quiet: bool) -> Result<()> {
    if !ffmpeg::available_encoders()?.contains("libx264") {
        fs::copy(input, output).with_context(|| format!("failed to save {}", output.display()))?;
        return Ok(());
    }

    if !quiet {
        output::status("Re-encoding video for player compatibility");
    }

    let log = tempfile::NamedTempFile::new().context("failed to create ffmpeg log file")?;
    let status = ffmpeg::ffmpeg()
        .arg("-i")
        .arg(input)
        // HiDPI captures can be odd-sized (500x375 at 1.25x scaling) and
        // libx264 rejects odd dimensions; pass-through when already even.
        .args(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"])
        .args([
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "20", "-preset", "veryfast", "-an",
        ])
        .arg(output)
        .stderr(Stdio::from(ffmpeg::log_file(log.path())?))
        .status()
        .context("failed to run ffmpeg")?;

    if !status.success() {
        return Err(ffmpeg::failed(&status, log.path()));
    }
    Ok(())
}

/// Estimated number of output frames: input duration × output fps. `None` if
/// the input duration can't be determined (no progress reporting then).
fn probe_total_frames(input: &Path, fps: u32) -> Option<u64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some((secs * fps as f64).round().max(1.0) as u64)
}

/// Run an ffmpeg command, reporting its `frame=` count from
/// `-progress pipe:1` through `report` (0..=1 when `total_frames > 0`).
fn run_ffmpeg(
    args: &[String],
    total_frames: u64,
    report: &mut dyn FnMut(f64),
    what: &str,
) -> Result<()> {
    let mut cmd = Command::new("ffmpeg");
    for arg in args {
        cmd.arg(arg);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run ffmpeg")?;

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        line.truncate(line.trim_end_matches(['\r', '\n']).len());
        if let Some(n) = line.strip_prefix("frame=")
            && let Ok(f) = n.parse::<u64>()
        {
            report(if total_frames > 0 {
                (f as f64 / total_frames as f64).min(1.0)
            } else {
                0.0
            });
        }
        line.clear();
    }

    if !child.wait().context("failed to wait for ffmpeg")?.success() {
        return Err(Error::Conversion(what.to_owned()).into());
    }
    Ok(())
}

/// Two-pass conversion with palette optimization. Pass 1 (palettegen) is
/// mapped to the first half of the progress range, pass 2 (paletteuse) to
/// the second — a heuristic split, not a measured ratio.
fn convert_optimized(
    input: &Path,
    output: &Path,
    base_filter: &str,
    quiet: bool,
    total_frames: u64,
    progress: &mut dyn FnMut(f64),
) -> Result<()> {
    let temp = TempDir::new().context("failed to create temp directory")?;
    let palette = temp.path().join("palette.png");

    if !quiet {
        output::info("generating optimal palette");
    }

    // Pass 1: Generate palette
    let args: Vec<String> = vec![
        "-y".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-vf".into(),
        format!("{}[x];[x]palettegen=stats_mode=diff", base_filter),
        palette.to_string_lossy().into_owned(),
    ];
    let mut p1 = |p: f64| progress(0.5 * p);
    run_ffmpeg(&args, total_frames, &mut p1, "palette generation failed")?;

    if !quiet {
        output::info("encoding with dithering");
    }

    // Pass 2: Apply palette with dithering
    let args: Vec<String> = vec![
        "-y".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-i".into(),
        palette.to_string_lossy().into_owned(),
        "-lavfi".into(),
        format!(
            "{}[x];[x][1:v]paletteuse=dither=floyd_steinberg",
            base_filter
        ),
        output.to_string_lossy().into_owned(),
    ];
    let mut p2 = |p: f64| progress(0.5 + 0.5 * p);
    run_ffmpeg(&args, total_frames, &mut p2, "GIF encoding failed")?;

    Ok(())
}

/// Single-pass fast conversion.
fn convert_fast(
    input: &Path,
    output: &Path,
    base_filter: &str,
    total_frames: u64,
    progress: &mut dyn FnMut(f64),
) -> Result<()> {
    let args: Vec<String> = vec![
        "-y".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-vf".into(),
        base_filter.to_owned(),
        output.to_string_lossy().into_owned(),
    ];
    run_ffmpeg(&args, total_frames, progress, "conversion failed")
}
