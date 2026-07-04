// build.rs — pre-flight checks that run before any dependencies.
//
// This crate has zero dependencies, so cargo builds it first.
// If a check fails, the build stops with a clear, actionable message.

fn main() {
    // ── Windows: ffmpeg-from-source needs `make` ──────────────────────────
    #[cfg(target_os = "windows")]
    if std::env::var("CARGO_FEATURE_FFMPEG_FROM_SOURCE").is_ok() {
        let has_make = std::process::Command::new("make")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_make {
            eprintln!();
            eprintln!("  Cannot build Thumbrella dependency ffmpeg without 'make'.");
            eprintln!("  To resolve either `winget install Git.Git` or provide your");
            eprintln!("  own ffmpeg build with `FFMPEG_DIR`:");
            eprintln!("    FFMPEG_DIR=/path/to/ffmpeg cargo build -p tier2");
            eprintln!();
            std::process::exit(1);
        }
    }
}
