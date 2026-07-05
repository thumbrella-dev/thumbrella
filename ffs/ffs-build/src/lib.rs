use std::ffi::CStr;

/// Describes which FFmpeg build is linked.
/// Call `init()` first, then read this for a string like "8.1.2 bundled-vcpkg".
pub fn build_string() -> String {
    let version = ffmpeg_version();
    format!("{version} {}", ffs_check::BUILD_STRING)
}

/// Initialize FFmpeg. Must be called once before any FFmpeg API use.
pub fn init() {
    ffmpeg_next::init().expect("failed to initialize FFmpeg");
}

/// Get the linked FFmpeg version string (e.g. "8.1.2").
fn ffmpeg_version() -> &'static str {
    // ffmpeg-next 8.x removed the version() function, so we go through FFI.
    // SAFETY: av_version_info() returns a static null-terminated string.
    unsafe {
        let ptr = ffmpeg_next::ffi::av_version_info();
        if ptr.is_null() {
            "unknown"
        } else {
            CStr::from_ptr(ptr).to_str().unwrap_or("unknown")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_string_combines_version_and_source() {
        init();
        let s = build_string();
        println!("FFmpeg build string: {s}");
        assert!(s.contains("bundled-vcpkg"), "expected 'bundled-vcpkg' in: {s}");
        // Version should look like "8.1.2" or similar
        assert!(s.starts_with(|c: char| c.is_ascii_digit()), "should start with version: {s}");
    }
}