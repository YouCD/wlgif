//! Native wlroots recording: a pure-Rust screencopy loop that pipes raw
//! RGBA frames to `ffmpeg` (already a hard dependency of the converter).
//!
//! Unlike [`crate::backend::wlr`], this backend needs no `wf-recorder` —
//! it speaks `zwlr_screencopy_manager_v1` directly (see
//! [`crate::screenshot`]), so it works on any wlroots compositor
//! (Sway, Hyprland, niri, dwl, ...).

use crate::backend::{Backend, RecordConfig, ffmpeg};
use crate::error::Error;
use crate::output;
use crate::region::Region;
use crate::screenshot::{self, Capture};
use anyhow::{Context, Result, bail};
use std::{
    fs::File,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub struct NativeWlrBackend;

impl NativeWlrBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for NativeWlrBackend {
    fn name(&self) -> &'static str {
        "wlroots"
    }

    fn is_available(&self) -> Result<()> {
        // `Capture::new` connects to the compositor and binds the screencopy
        // manager — it fails on compositors without the protocol.
        Capture::new()
            .map(|_| ())
            .map_err(|e| anyhow::Error::from(Error::Recording(e)))?;
        // The frames go into ffmpeg, so an encoder-less ffmpeg build makes
        // this backend unusable; detect it here so `detect()` can fall
        // through to a backend that works.
        ffmpeg::pick_video_encoder()?;
        Ok(())
    }

    fn record(&self, region: Option<&Region>, output: &Path, config: &RecordConfig) -> Result<()> {
        super::reset_ctrl_c();
        super::ensure_ctrl_c_handler()?;

        // Resolve the encoder before capturing anything: an unknown encoder
        // must fail in milliseconds, not after the whole recording.
        let encoder = ffmpeg::pick_video_encoder()?;

        // Stop can be requested via Ctrl-C or externally (GUI stop button).
        let stopped = || {
            super::ctrl_c_pressed()
                || config
                    .stop
                    .as_ref()
                    .is_some_and(|s| s.load(Ordering::SeqCst))
        };

        let mut caps =
            Capture::new().map_err(|e| anyhow::anyhow!("failed to connect to compositor: {e}"))?;
        let (wl_output, (x, y, w, h)) =
            screenshot::pick_output(caps.outputs(), region).map_err(anyhow::Error::msg)?;

        // Capture the first frame up front: it determines the exact pixel
        // size (the compositor may clip the requested region).
        let first = caps
            .capture_output_region(&wl_output, x, y, w, h, true)
            .map_err(Error::Recording)?;
        let (fw, fh) = (first.width, first.height);
        let first_rgba = screenshot::to_rgba8(&first).map_err(Error::Recording)?;

        if !config.quiet {
            output::recording(config.duration);
        }

        let fps = config.fps.max(1);
        // ffmpeg's stderr goes to a file so a failure reports the real reason
        // instead of a bare exit status.
        let log = tempfile::NamedTempFile::new().context("failed to create ffmpeg log file")?;
        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-video_size", &format!("{fw}x{fh}")])
            .args(["-framerate", &fps.to_string()])
            .args(["-i", "pipe:0"])
            // HiDPI scaling makes the captured size odd (a 400x300 logical
            // region at 1.25x is 500x375), and every fallback encoder needs
            // even dimensions. This is a pass-through when they are even.
            .args(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"])
            .args(["-c:v", encoder.name])
            .args(encoder.flags)
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                File::create(log.path()).context("failed to open ffmpeg log file")?,
            ))
            .spawn()
            .context("failed to start ffmpeg — is it installed?")?;

        let mut stdin = child
            .stdin
            .take()
            .context("failed to pipe frames into ffmpeg")?;

        // ffmpeg rejects an encoder/container mismatch within the first
        // few milliseconds. Polling once here costs 150 ms; not polling
        // costs the entire recording.
        std::thread::sleep(Duration::from_millis(150));
        if let Some(status) = child.try_wait().context("failed to poll ffmpeg")? {
            drop(stdin);
            return Err(ffmpeg::failed(&status, log.path()));
        }

        stdin
            .write_all(&first_rgba)
            .with_context(|| ffmpeg::write_failed(log.path()))?;

        let started = Instant::now();
        let deadline = (config.duration > 0.0)
            .then(|| started + Duration::from_secs_f64(config.duration as f64));

        let mut frame = 1u64;
        loop {
            if stopped() {
                break;
            }
            if let Some(d) = deadline
                && Instant::now() >= d
            {
                break;
            }

            let cap = caps
                .capture_output_region(&wl_output, x, y, w, h, true)
                .map_err(Error::Recording)?;
            if (cap.width, cap.height) != (fw, fh) {
                bail!("capture size changed mid-recording");
            }
            let rgba = screenshot::to_rgba8(&cap).map_err(Error::Recording)?;
            stdin
                .write_all(&rgba)
                .with_context(|| ffmpeg::write_failed(log.path()))?;
            frame += 1;

            // Pace to the frame deadline; if the capture itself is slower
            // than the target fps, the loop naturally runs at capture speed.
            let next = started + Duration::from_secs_f64(frame as f64 / fps as f64);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
        }

        // Closing stdin tells ffmpeg to finalize the file.
        drop(stdin);
        let status = child.wait().context("failed to wait for ffmpeg")?;
        if !status.success() {
            return Err(ffmpeg::failed(&status, log.path()));
        }

        Ok(())
    }
}
