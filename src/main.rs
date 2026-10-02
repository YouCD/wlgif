mod backend;
mod cli;
mod converter;
mod error;
#[cfg(feature = "gui")]
mod gui;
mod output;
mod region;
mod screenshot;

use anyhow::{Context, Result, bail};
use cli::Args;
use error::Error;
use region::Region;
use std::fs;
use tempfile::TempDir;

use crate::backend::RecordConfig;

fn validate_output(args: &Args) -> Result<()> {
    // TODO: Improve extension check via file type
    match args.output.extension().and_then(|e| e.to_str()) {
        Some("gif") => Ok(()),
        Some(ext) => bail!("output must be a .gif file, got .{}", ext),
        None => bail!("output must be a .gif file"),
    }
}

fn get_region(args: &Args) -> Result<Option<Region>> {
    match &args.geometry {
        Some(g) => Ok(Some(Region::from_geometry(g)?)),
        // TODO: Improve matching
        None if args.backend.as_deref() == Some("xdg-desktop-portal") => {
            // Portal backend uses its own selection UI
            Ok(None)
        }
        None => Ok(Some(region::select_interactive(args.quiet)?)),
    }
}

fn main() -> Result<()> {
    let args = Args::parse_args();

    #[cfg(feature = "gui")]
    if args.gui {
        gui::run(&args.output).map_err(|e| anyhow::anyhow!("failed to run GUI: {e:?}"))?;
        return Ok(());
    }

    validate_output(&args)?;

    // Validate the geometry string up front so a typo (e.g. `-gui`, which
    // clap reads as `-g ui`) fails with a clear message instead of a
    // confusing backend error.
    if let Some(g) = &args.geometry {
        Region::from_geometry(g)?;
    }

    let backend = match &args.backend {
        Some(name) => backend::by_name(name)?,
        None => backend::detect()?,
    };

    if !args.quiet {
        output::status(&format!("Using {} backend", backend.name()));
    }

    if let Err(err) = backend.is_available() {
        bail!(
            "backend '{}' is not available on this system: {err}",
            backend.name()
        );
    }

    let region = get_region(&args)?;

    if !args.quiet
        && let Some(ref r) = region
    {
        output::status(&format!("Region: {}", r));
    }

    let temp = TempDir::new().context("failed to create temp directory")?;
    let video = temp.path().join("capture.mp4");
    let config = RecordConfig {
        fps: args.fps,
        duration: args.duration,
        quiet: args.quiet,
        stop: None,
    };

    backend.record(region.as_ref(), &video, &config)?;

    // A missing or zero-byte capture both mean nothing was recorded.
    let video_size = fs::metadata(&video).map(|m| m.len()).unwrap_or(0);
    if video_size == 0 {
        return Err(Error::EmptyRecording.into());
    }

    converter::to_gif(
        &video,
        &args.output,
        args.fps,
        args.width,
        !args.fast,
        args.quiet,
        None,
    )?;

    if args.keep_video {
        let kept = args.output.with_extension("mp4");
        fs::copy(&video, &kept).context("failed to save video")?;
        if !args.quiet {
            output::info(&format!("Video: {}", kept.display()));
        }
    }

    let size = fs::metadata(&args.output).map(|m| m.len()).unwrap_or(0);

    if !args.quiet {
        output::divider();
        output::summary(&args.output, size);
    }

    Ok(())
}
