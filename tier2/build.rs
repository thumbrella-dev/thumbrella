// build.rs for tier2
//
// Platform-conditional FFmpeg linking.
//
// If FFMPEG_DIR is set (env var or .cargo/config.toml), we link against that
// directory — static or shared, as long as it's the right version.
//
// If FFMPEG_DIR is not set, we default to target/ffmpeg-static under the
// workspace root and print clear instructions when the directory is missing.
//
// ── Getting FFmpeg ──────────────────────────────────────────────────────
// Linux:   ./tier2/build_static_ffmpeg.sh           (source build → static .a)
// Windows: powershell -File tier2/download_ffmpeg_windows.ps1   (BtbN prebuilt)
// macOS:   Set FFMPEG_DIR to a static FFmpeg install (no auto-build yet)

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    let ffmpeg_dir = std::env::var("FFMPEG_DIR")
        .unwrap_or_else(|_| default_ffmpeg_dir());
    let lib_dir = format!("{ffmpeg_dir}/lib");

    // Verify the directory exists.  Don't auto-build — that's fragile inside
    // build.rs.  Instead, print clear instructions.
    if !std::path::Path::new(&lib_dir).exists() {
        eprintln!();
        eprintln!("  FFmpeg not found.");
        eprintln!("  Looking for: {lib_dir}");
        eprintln!();
        eprintln!("  ── Linux ──");
        eprintln!("  Build a minimal static FFmpeg:");
        eprintln!("    ./tier2/build_static_ffmpeg.sh");
        eprintln!();
        eprintln!("  ── Windows ──");
        eprintln!("  Download prebuilt MSVC FFmpeg:");
        eprintln!("    powershell -File tier2/download_ffmpeg_windows.ps1");
        eprintln!();
        eprintln!("  ── macOS ──");
        eprintln!("  Install via Homebrew, then set FFMPEG_DIR:");
        eprintln!("    brew install ffmpeg");
        eprintln!("    export FFMPEG_DIR=/opt/homebrew/opt/ffmpeg");
        eprintln!();
        eprintln!("  Or set FFMPEG_DIR to your own installation.");
        eprintln!();
        std::process::exit(1);
    }

    // ffmpeg-sys-next links avcodec/avformat/avutil/swscale/swresample.
    // We add the transitive system deps per platform.

    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");
        println!("cargo:rustc-link-search=native={lib_dir}");
        println!("cargo:rustc-link-lib=static=z");
        println!("cargo:rustc-link-lib=static=bz2");
        println!("cargo:rustc-link-lib=static=lzma");
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=atomic");
    }

    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-search=native={lib_dir}");
        // MSVC system libs commonly referenced by FFmpeg static builds.
        println!("cargo:rustc-link-lib=bcrypt");
        println!("cargo:rustc-link-lib=secur32");
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=ole32");
    }

    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-search=native={lib_dir}");
        println!("cargo:rustc-link-lib=static=bz2");
        println!("cargo:rustc-link-lib=static=lzma");
        println!("cargo:rustc-link-lib=static=z");
        println!("cargo:rustc-link-lib=static=iconv");
        println!("cargo:rustc-link-lib=framework=AudioToolbox");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
    }
}

fn default_ffmpeg_dir() -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_string());
    // tier2/ → go up one level for workspace root → target/ffmpeg-static
    let root = std::path::Path::new(&manifest)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    root.join("target").join("ffmpeg-static")
        .to_string_lossy()
        .to_string()
}
