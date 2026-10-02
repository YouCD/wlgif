//! Native wlroots recording: a pure-Rust screencopy loop that pipes raw
//! RGBA frames to `ffmpeg` (already a hard dependency of the converter).
//!
//! Unlike [`crate::backend::wlr`], this backend needs no `wf-recorder` —
//! it speaks `zwlr_screencopy_manager_v1` directly (see
//! [`crate::screenshot`]), so it works on any wlroots compositor
//! (Sway, Hyprland, niri, dwl, ...).

use crate::backend::{Backend, RecordConfig};
use crate::error::Error;
use crate::output;
use crate::region::Region;
use crate::screenshot::{self, Capture};
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use wayland_client::protocol::wl_output::WlOutput;

pub struct NativeWlrBackend;

impl NativeWlrBackend {
    pub fn new() -> Self {
        Self
    }

    /// Pick the output containing the region's top-left corner and convert
    /// the global logical region into output-local coordinates (clamped to
    /// the output's extents). Without a region, capture the first output.
    fn pick_output(
        caps: &Capture,
        region: Option<&Region>,
    ) -> Result<(WlOutput, (i32, i32, i32, i32))> {
        let outputs = caps.outputs();
        if outputs.is_empty() {
            bail!("compositor reports no outputs");
        }
        match region {
            Some(r) => {
                let (rx, ry) = (r.x as i32, r.y as i32);
                for (o, info) in outputs {
                    let (ox, oy) = (info.x, info.y);
                    let (ow, oh) = (info.width as i32, info.height as i32);
                    if rx >= ox && ry >= oy && rx < ox + ow && ry < oy + oh {
                        let x = rx - ox;
                        let y = ry - oy;
                        let w = (r.width as i32).min(ow - x).max(1);
                        let h = (r.height as i32).min(oh - y).max(1);
                        return Ok((o.clone(), (x, y, w, h)));
                    }
                }
                bail!("region ({}, {}) is outside every known output", r.x, r.y)
            }
            None => {
                let (o, info) = &outputs[0];
                Ok((o.clone(), (0, 0, info.width as i32, info.height as i32)))
            }
        }
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
            .map_err(|e| Error::Recording(e).into())
    }

    fn record(&self, region: Option<&Region>, output: &Path, config: &RecordConfig) -> Result<()> {
        super::reset_ctrl_c();
        super::ensure_ctrl_c_handler()?;

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
        let (wl_output, (x, y, w, h)) = Self::pick_output(&caps, region)?;

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
        let mut child = Command::new("ffmpeg")
            .args(["-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-video_size", &format!("{fw}x{fh}")])
            .args(["-framerate", &fps.to_string()])
            .args(["-i", "pipe:0", "-c:v", "libx264rgb", "-crf", "18"])
            .arg(output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start ffmpeg — is it installed?")?;

        let mut stdin = child
            .stdin
            .take()
            .context("failed to pipe frames into ffmpeg")?;
        stdin
            .write_all(&first_rgba)
            .context("failed to write frame to ffmpeg")?;

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
                .context("failed to write frame to ffmpeg")?;
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
            return Err(Error::Recording(format!("ffmpeg exited: {status}")).into());
        }

        Ok(())
    }
}
