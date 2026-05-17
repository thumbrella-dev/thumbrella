//! Static binary assets embedded at compile time.
//!
//! Exposing these as a single `pub mod` means every layer that builds on top of
//! tier1 (CF Workers, CLI, future adapters) can reference the *same* bytes.
//! A single `include_bytes!` per asset guarantees the bytes are embedded once
//! in the final binary, regardless of how many layers reference them.

/// Background image used for thumbnail compositing (PNG).
pub static BACKGROUND_PNG: &[u8] =
    include_bytes!("../assets/background.png");

/// General-purpose placeholder thumbnail returned when a thumbnail cannot be
/// produced (e.g. unsupported media type, rate-limited, unavailable).  JPEG.
pub static PLACEHOLDER_GENERAL_JPG: &[u8] =
    include_bytes!("../assets/placeholder_general.jpg");

/// Error placeholder thumbnail returned when the pipeline itself fails.  JPEG.
pub static PLACEHOLDER_ERROR_JPG: &[u8] =
    include_bytes!("../assets/placeholder_error.jpg");
