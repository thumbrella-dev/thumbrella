// build.rs for tier2
//
// FFmpeg's static .a files have unresolved references to a small set of
// system libraries.  Our custom build (scripts/build-ffmpeg-static.sh)
// was configured with --enable-zlib --enable-bzlib --enable-lzma, so
// we need to supply those archives.
//
// Static: z (zlib), bz2, lzma — all have .a files on the build host.
// Dynamic (glibc, accepted): m, atomic — thin stubs, not worth a fat .a.
//
// The libstdc++ dependency does NOT appear in our minimal FFmpeg build
// because we disabled everything that pulls in C++ code.
//
// Note: ffmpeg-sys-next's own build.rs already emits:
//   cargo:rustc-link-lib=static=avcodec
//   cargo:rustc-link-lib=static=avformat
//   cargo:rustc-link-lib=static=avutil
//   cargo:rustc-link-lib=static=swscale
//   cargo:rustc-link-lib=static=swresample
// So we only need to add the transitive deps here.

fn main() {
    // Only re-run this script if build.rs itself changes.  Without this,
    // Cargo re-runs on every file-system event — a problem on Windows-hosted
    // bind mounts where NTFS timestamps fire spuriously and trigger relinks.
    println!("cargo:rerun-if-changed=build.rs");

    // Tell the linker where to find the system static archives.
    // On Debian/Ubuntu amd64 these land in /usr/lib/x86_64-linux-gnu.
    println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");

    // Also search /opt/ffmpeg-static/lib for external static libs (dav1d, etc.).
    println!("cargo:rustc-link-search=native=/opt/ffmpeg-static/lib");

    // dav1d (AV1/AVIF decoder) — optional.  Only linked when the static
    // archive exists (built by scripts/build-ffmpeg-static.sh alongside FFmpeg).
    let has_dav1d = std::path::Path::new("/opt/ffmpeg-static/lib/libdav1d.a").exists();
    let static_libs = if has_dav1d {
        "-Wl,-Bstatic,-lz,-lbz2,-llzma,-ldav1d,-Bdynamic"
    } else {
        "-Wl,-Bstatic,-lz,-lbz2,-llzma,-Bdynamic"
    };

    // These must appear *after* the FFmpeg archives on the linker command
    // line so GNU ld can resolve the forward references from libavcodec etc.
    // into these compression libraries.  cargo:rustc-link-arg values are
    // always appended after all cargo:rustc-link-lib values (including those
    // emitted by ffmpeg-sys-next's build script), so this single -Wl flag
    // lands in the right position without any --start-group tricks.
    //
    // -Bstatic / -Bdynamic scope the mode so only these archives are
    // pulled as .a files; the linker reverts to shared-library search after.
    println!("cargo:rustc-link-arg={static_libs}");

    // These are glibc-provided; keep dynamic (statically linking glibc is
    // fragile and ties the binary to the build host's glibc version).
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rustc-link-lib=dylib=atomic");
}
