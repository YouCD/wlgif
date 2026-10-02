use clap::Parser;
use std::path::PathBuf;

const BUILD_REVISION: &str = env!("BUILD_REVISION");
const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const CUSTOM_VERSION: &str = const_format::formatcp!("{PKG_VERSION}+{BUILD_REVISION}");

#[derive(Parser, Debug)]
#[command(name = "wlgif")]
#[command(version = CUSTOM_VERSION)]
#[command(about = "Record a region of your Wayland screen as a GIF")]
#[command(after_help = "\x1b[1mExamples:\x1b[0m
  wlgif                     Select region, record for 5s (default behavior)
  wlgif -d 10               Record for 10 seconds
  wlgif -d 0                Manual stop with Ctrl+C
  wlgif -g 800x600+100+100  Skip selection, use geometry
  wlgif --fps 30 -w 640     30fps, scaled to 640px wide
  wlgif --backend xdg-desktop-portal    Use XDG portal backend (cross-compositor)
  wlgif --backend wlroots               Use native screencopy backend (wlroots)
  wlgif --backend wf-recorder           Use the wf-recorder-based backend

\x1b[1mDependencies:\x1b[0m
  native (wlroots):  ffmpeg only (Sway, Hyprland, niri, ...)
  wf-recorder:       slurp, wf-recorder, ffmpeg
  portal:            xdg-desktop-portal, pipewire, gstreamer")]
pub struct Args {
    /// Recording backend (auto-detected if not specified)
    #[arg(short, long, value_name = "NAME")]
    pub backend: Option<String>,

    /// Output GIF file path
    #[arg(short, long, default_value = "output.gif")]
    pub output: PathBuf,

    /// Recording duration in seconds (0 = manual stop with Ctrl+C)
    #[arg(short, long, default_value = "5", value_name = "SECS", value_parser = non_negative_secs)]
    pub duration: f32,

    /// Frames per second (10-30 recommended)
    #[arg(short, long, default_value = "15", value_name = "FPS", value_parser = fps_range)]
    pub fps: u32,

    /// Region geometry, skip interactive selection (WxH+X+Y)
    #[arg(short, long, value_name = "WxH+X+Y")]
    pub geometry: Option<String>,

    /// Scale output width in pixels (height auto-calculated)
    #[arg(short, long, value_name = "PX")]
    pub width: Option<u32>,

    /// Skip palette optimization (faster, larger file)
    #[arg(long)]
    pub fast: bool,

    /// Keep intermediate video file
    #[arg(long)]
    pub keep_video: bool,

    /// Suppress status output
    #[arg(short, long)]
    pub quiet: bool,

    /// Launch the graphical interface
    #[arg(long)]
    pub gui: bool,
}

/// Validate a frames-per-second value in the 1..=60 range.
fn fps_range(value: &str) -> Result<u32, String> {
    let fps: u32 = value
        .parse()
        .map_err(|_| format!("'{}' is not a number", value))?;
    if !(1..=60).contains(&fps) {
        return Err(format!("'{}' is out of range 1..=60", value));
    }
    Ok(fps)
}

/// Validate a non-negative recording duration (seconds).
fn non_negative_secs(value: &str) -> Result<f32, String> {
    let secs: f32 = value
        .parse()
        .map_err(|_| format!("'{}' is not a number", value))?;
    if secs.is_nan() || secs < 0.0 {
        return Err(format!("'{}' must be a non-negative number", value));
    }
    Ok(secs)
}

impl Args {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
