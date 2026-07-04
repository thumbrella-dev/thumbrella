// build.rs for tier3
//
// Tier 3 depends on tier2 → ffmpeg-sys-next.  The ffmpeg-sys-next build
// script emits rustc-link-lib directives that are inherited by dependents,
// so we do not need to repeat those here.  We only add platform-specific
// transitive system deps that FFmpeg's static libraries reference.
//
// On Windows, everything is handled by tier2's vcpkg setup.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");
        println!("cargo:rustc-link-search=native=/opt/ffmpeg-static/lib");

        let has_dav1d = std::path::Path::new("/opt/ffmpeg-static/lib/libdav1d.a").exists();
        if has_dav1d {
            println!("cargo:rustc-link-lib=static=dav1d");
        }
        println!("cargo:rustc-link-lib=static=z");
        println!("cargo:rustc-link-lib=static=bz2");
        println!("cargo:rustc-link-lib=static=lzma");

        // glibc-provided; keep dynamic.
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=atomic");
    }
}
