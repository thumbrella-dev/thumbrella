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
    // Tell the linker where to find the system static archives.
    // On Debian/Ubuntu amd64 these land in /usr/lib/x86_64-linux-gnu.
    println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");

    // Transitive deps of our minimal FFmpeg build.
    // Link these dynamically so GNU ld ordering constraints don't matter;
    // all three have .so files on Debian/Ubuntu.
    println!("cargo:rustc-link-lib=dylib=z");
    println!("cargo:rustc-link-lib=dylib=bz2");
    println!("cargo:rustc-link-lib=dylib=lzma");

    // These are glibc-provided; linking them dynamically is fine and
    // avoids the pitfalls of statically linking libpthread.
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rustc-link-lib=dylib=atomic");
}
