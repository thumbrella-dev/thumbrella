//! Canonical thumbnail output spec.
//!
//! The profile is a product decision, not a caller parameter.  There is exactly
//! one canonical profile at any given time.  The `version` field is incremented
//! whenever any value changes so that cache entries from an older profile are
//! automatically invalidated.

use serde::{Deserialize, Serialize};

/// The canonical thumbnail output configuration.
///
/// All thumbnails produced by this service conform to this config.  Callers
/// cannot override it; that is an intentional product constraint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailConfig {
    /// Monotonically increasing version.  Bust this when any field changes.
    pub version: u32,
    /// Exact output width in pixels.  All thumbnails are scaled to fill this size.
    pub exact_width: u32,
    /// Exact output height in pixels.
    pub exact_height: u32,
    /// JPEG quality 1–100 for photographic content.
    pub jpeg_quality: u8,
    /// JPEG quality for pixel-art / icon content (typically higher to avoid
    /// visible DCT artifacts on hard edges).
    pub pixel_art_quality: u8,
    /// Background colour used when flattening transparency (RGB).
    pub background_rgb: [u8; 3],
    /// Vignette darkening strength at image edges, 0.0 (none) to 1.0 (full).
    pub vignette_strength: f32,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self::CANONICAL
    }
}

impl ThumbnailConfig {
    /// The one canonical config.  Update `version` whenever any value changes.
    pub const CANONICAL: Self = Self {
        version: 2,
        exact_width: 250,
        exact_height: 200,
        jpeg_quality: 46,
        pixel_art_quality: 18,
        background_rgb: [255, 255, 255],
        vignette_strength: 0.25,
    };
}
