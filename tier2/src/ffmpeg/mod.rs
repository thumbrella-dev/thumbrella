// Pre-generated FFmpeg 7.1 FFI bindings, per platform.
//
// These are generated once with bindgen and committed, so tier2 builds
// do not need libclang at runtime.  When upgrading FFmpeg, regenerate:
//
//   cargo clean -p ffmpeg-sys-next && cargo build -p tier2
//   cp target/debug/build/ffmpeg-sys-next-*/out/bindings.rs \
//      tier2/src/ffmpeg/<platform>.rs
//
// Then remove the `#![allow(...)]` lines from the copied file.

#[cfg(target_os = "linux")]
mod linux {
    #![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, unused)]
    include!("linux_x64.rs");
}
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "windows")]
compile_error!("\
    Pre-generated FFmpeg bindings not yet available for Windows.\n\
    Generate them once:\n\
    1. winget install LLVM.LLVM\n\
    2. cargo install bindgen-cli\n\
    3. FFMPEG_DIR=target/ffmpeg-static bash tier2/src/ffmpeg/generate_bindings.sh\n\
    Then add `#[cfg(target_os = \"windows\")]` block to this file.");
