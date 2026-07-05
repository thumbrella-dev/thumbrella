/// Build string describing the linked FFmpeg.
/// Set by build.rs via cargo:rustc-env after environment validation.
pub const BUILD_STRING: &str = env!("FFS_BUILD_STRING");
