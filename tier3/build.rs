// build.rs for tier3
//
// Same linker setup as tier2: supply the system static archives that FFmpeg's
// .a files reference (zlib, bz2, lzma, optionally dav1d) plus the required
// dynamic system libs (m, atomic).
//
// Tier 3 depends on tier2 which depends on ffmpeg-sys-next.  The
// ffmpeg-sys-next build script emits rustc-link-lib directives that are
// inherited by dependents, so we do not need to repeat those here.  We
// only need the transitive system deps.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");
    println!("cargo:rustc-link-search=native=/opt/ffmpeg-static/lib");

    let has_dav1d = std::path::Path::new("/opt/ffmpeg-static/lib/libdav1d.a").exists();
    if has_dav1d {
        println!("cargo:rustc-link-lib=static=dav1d");
    }
    println!("cargo:rustc-link-lib=static=z");
    println!("cargo:rustc-link-lib=static=bz2");
    println!("cargo:rustc-link-lib=static=lzma");

    // These are glibc-provided; keep dynamic (statically linking glibc is
    // fragile and ties the binary to the build host's glibc version).
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rustc-link-lib=dylib=atomic");
}
