use anyhow::{Result, bail};
use std::path::Path;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use crate::region::Region;
pub mod ffmpeg;
mod native;
mod wlr;
mod xdg_portal;

pub struct RecordConfig {
    pub fps: u32,
    pub duration: f32,
    pub quiet: bool,
    /// Optional external stop request (set by the GUI stop button).
    pub stop: Option<Arc<AtomicBool>>,
}

/// A screencast backend
pub trait Backend {
    /// Backend name
    fn name(&self) -> &'static str;

    /// Check if this backend is available
    fn is_available(&self) -> Result<()>;

    /// Record a screen region to a video file.
    ///
    /// For backends that don't support region selection (like xdg-portal),
    /// the region parameter may be ignored and full-screen capture used.
    fn record(&self, region: Option<&Region>, output: &Path, config: &RecordConfig) -> Result<()>;
}

/// Detect and return the best available backend
pub fn detect() -> Result<Box<dyn Backend>> {
    // Native screencopy first: no external tools needed beyond ffmpeg.
    let native = native::NativeWlrBackend::new();
    if native.is_available().is_ok() {
        return Ok(Box::new(native));
    }

    let wlr = wlr::WlrBackend::new();
    if wlr.is_available().is_ok() {
        return Ok(Box::new(wlr));
    }

    let xdg_portal = xdg_portal::XDGPortalBackend::new();
    if xdg_portal.is_available().is_ok() {
        return Ok(Box::new(xdg_portal));
    }

    anyhow::bail!(
        "no recording backend available\n  \
         install one of:\n    \
         - nothing, if you run a wlroots compositor (Sway, Hyprland, niri) —\n    \
           the native screencopy backend should work as-is\n    \
         - wf-recorder (wlroots compositors)\n    \
         - xdg-desktop-portal + pipewire + gstreamer"
    )
}

// Ctrl-C handling. `ctrlc::set_handler` can only be installed once per
// process, so every recording shares one handler and one flag instead of
// trying to re-register (which failed with `MultipleHandlers` on the
// second recording).
static CTRL_C: OnceLock<AtomicBool> = OnceLock::new();

/// Whether Ctrl-C has been pressed since the last `reset_ctrl_c`.
pub(crate) fn ctrl_c_pressed() -> bool {
    CTRL_C
        .get_or_init(|| AtomicBool::new(false))
        .load(Ordering::SeqCst)
}

/// Clear the shared flag at the start of a recording.
pub(crate) fn reset_ctrl_c() {
    CTRL_C
        .get_or_init(|| AtomicBool::new(false))
        .store(false, Ordering::SeqCst);
}

/// Install the shared Ctrl-C handler; a previously installed handler is
/// fine (it writes the same flag).
pub(crate) fn ensure_ctrl_c_handler() -> Result<()> {
    match ctrlc::set_handler(|| {
        CTRL_C
            .get_or_init(|| AtomicBool::new(false))
            .store(true, Ordering::SeqCst);
    }) {
        Ok(()) => Ok(()),
        Err(ctrlc::Error::MultipleHandlers) => Ok(()),
        Err(e) => bail!("failed to set signal handler: {e}"),
    }
}

/// Get a specific backend by name
pub fn by_name(name: &str) -> Result<Box<dyn Backend>> {
    match name {
        "xdg-desktop-portal" | "xdg-portal" | "xdg" | "portal" => {
            Ok(Box::new(xdg_portal::XDGPortalBackend::new()))
        }
        "wlroots" | "wlr" | "native" => Ok(Box::new(native::NativeWlrBackend::new())),
        "wf-recorder" => Ok(Box::new(wlr::WlrBackend::new())),
        _ => anyhow::bail!("unknown backend: {}", name),
    }
}
