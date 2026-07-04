// build.rs for tier2
//
// FFmpeg discovery and linking is handled by ffmpeg-sys-next:
//
//   Default:
//     Uses FFMPEG_DIR from .cargo/config.toml (points to workspace-local vcpkg
//     on Windows).  Set your own FFMPEG_DIR to override.
//
//   ffmpeg-from-source (opt-in):
//     ffmpeg-sys-next clones FFmpeg from git, configures, builds, and links
//     statically.  Requires `make` on all platforms.
//
// This file adds platform-specific system library deps and pre-flight checks.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");

    // ── FFMPEG_DIR vs ffmpeg-from-source conflict ─────────────────────────
    if std::env::var("FFMPEG_DIR").is_ok() {
        if std::env::var("CARGO_FEATURE_FFMPEG_FROM_SOURCE").is_ok() {
            eprintln!();
            eprintln!("  note: FFMPEG_DIR is set, but ffmpeg-from-source feature is also");
            eprintln!("  enabled — the source build takes priority in ffmpeg-sys-next.");
            eprintln!();
            eprintln!("  To use your external FFmpeg, drop the ffmpeg-from-source feature:");
            eprintln!("    cargo build -p tier2 --no-default-features -F native");
            eprintln!();
        }
    }

    // ── Windows system libraries ──────────────────────────────────────────
    // Safe to emit unconditionally — the linker ignores duplicates.
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=bcrypt");
        println!("cargo:rustc-link-lib=secur32");
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=user32");
        println!("cargo:rustc-link-lib=ole32");
        // Additional Windows libs needed by FFmpeg's media foundation and schannel support
        println!("cargo:rustc-link-lib=crypt32");
        println!("cargo:rustc-link-lib=ncrypt");
        println!("cargo:rustc-link-lib=mfplat");
        println!("cargo:rustc-link-lib=strmiids");
        println!("cargo:rustc-link-lib=mfuuid");
    }
}

