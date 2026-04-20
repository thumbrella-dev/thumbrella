//! Canonical thumbnail output profile.
//!
//! This is a product decision, not a caller parameter. It is versioned so
//! cache entries can be invalidated when the profile changes.

use serde::{Deserialize, Serialize};

/// The one canonical output format for all thumbnails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailProfile {
    /// Profile version — increment when any field changes to bust cached results.
    pub version: u32,
    /// Max output width in pixels.
    pub width: u32,
    /// Max output height in pixels.
    pub height: u32,
    /// JPEG quality 1-100.
    pub quality: u8,
    /// JPEG quality used in pixel-art mode.
    pub pixel_art_quality: u8,
    /// Base vignette strength used by the color pipeline.
    pub vignette_strength: f32,
}

impl Default for ThumbnailProfile {
    fn default() -> Self {
        Self {
            version: 1,
            width: 250,
            height: 200,
            quality: 50,
            pixel_art_quality: 15,
            vignette_strength: 0.25,
        }
    }
}
