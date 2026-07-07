fn main() {
    // ffmpeg-sys-next links the FFmpeg archives, but does not always propagate
    // transitive compression libs from pkg-config for static archive linking.
    // Link them explicitly so symbols used by libavcodec/libavformat resolve.
    #[cfg(target_os = "linux")]
    {
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
        let triplet_lib = format!("/usr/lib/{arch}-linux-gnu");
        let triplet_lib_alt = format!("/lib/{arch}-linux-gnu");

        // Ensure static archives from distro dev packages are discoverable.
        println!("cargo:rustc-link-search=native={triplet_lib}");
        println!("cargo:rustc-link-search=native={triplet_lib_alt}");

        println!("cargo:rustc-link-lib=static=z");
        println!("cargo:rustc-link-lib=static=lzma");
        println!("cargo:rustc-link-lib=static=bz2");
    }
}