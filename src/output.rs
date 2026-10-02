use colored::Colorize;
use std::path::Path;

const RULE: &str = "──────────────────────────────────────────────";

/// Print a main step (cyan arrow).
pub fn status(msg: &str) {
    eprintln!("{} {}", "▸".cyan().bold(), msg);
}

/// Print a sub-step (dimmed dot).
pub fn info(msg: &str) {
    eprintln!("  {} {}", "·".dimmed(), msg);
}

/// Print a success line (green check).
pub fn success(msg: &str) {
    eprintln!("{} {}", "✓".green().bold(), msg);
}

/// Print a warning (yellow).
#[allow(dead_code)]
pub fn warn(msg: &str) {
    eprintln!("{} {}", "⚠".yellow().bold(), msg);
}

/// Print a dimmed horizontal rule.
pub fn divider() {
    eprintln!("{}", RULE.dimmed());
}

/// Print recording status with duration info.
pub fn recording(duration: f32) {
    eprintln!();
    if duration > 0.0 {
        eprintln!(
            "{} {} for {:.1}s {}",
            "●".red().bold(),
            "Recording".bold(),
            duration,
            "·  Ctrl+C to stop early".dimmed()
        );
    } else {
        eprintln!(
            "{} {} {}",
            "●".red().bold(),
            "Recording".bold(),
            "·  Ctrl+C to stop".dimmed()
        );
    }
}

/// Print the final summary after successful GIF creation.
pub fn summary(path: &Path, size_bytes: u64) {
    let size_kb = size_bytes as f64 / 1024.0;
    let size_str = if size_kb >= 1024.0 {
        format!("{:.2} MB", size_kb / 1024.0)
    } else {
        format!("{:.1} KB", size_kb)
    };
    success(&format!("Saved {} {}", path.display(), size_str.dimmed()));
}
