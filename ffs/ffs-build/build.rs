// Emit linker directives for the static FFmpeg libraries found via vcpkg.
// VCPKG_ROOT is set by .cargo/config.toml (written by ffs/build-windows.ps1).
// ffs-check/build.rs validated the environment before this runs.

fn main() {
    #[cfg(windows)]
    {
        let _lib = vcpkg::Config::new()
            .target_triplet("x64-windows-static")
            .find_package("ffmpeg")
            .expect("ffmpeg not found in vcpkg tree");

        let ffmpeg_libs = ["avcodec", "avdevice", "avfilter", "avformat", "avutil", "swresample", "swscale"];
        for name in &ffmpeg_libs {
            println!("cargo:rustc-link-lib=static={}", name);
        }

        let system_libs = [
            "psapi", "uuid", "oleaut32", "shlwapi", "gdi32", "vfw32", "secur32", "ncrypt", "crypt32",
            "ws2_32", "mfuuid", "strmiids", "ole32", "user32", "bcrypt",
        ];
        for name in &system_libs {
            println!("cargo:rustc-link-lib={}", name);
        }
    }
}
