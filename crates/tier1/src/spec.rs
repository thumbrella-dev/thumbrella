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
    /// Maximum source dimension (in pixels) in *both* width and height for
    /// pixel-art mode.  Sources larger than this in either axis are treated as
    /// photographic and encoded at `jpeg_quality` with Triangle resize.
    pub pixel_art_max_px: u32,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self::CANONICAL
    }
}

impl ThumbnailConfig {
    /// The one canonical config.  Update `version` whenever any value changes.
    pub const CANONICAL: Self = Self {
        version: 4,
        exact_width: 250,
        exact_height: 200,
        jpeg_quality: 60,
        pixel_art_quality: 18,
        background_rgb: [255, 255, 255],
        vignette_strength: 0.25,
        pixel_art_max_px: 100,
    };
}

// ── ShortcutLimits ────────────────────────────────────────────────────────────

/// I/O and decode budget limits for the shortcut pipeline.
///
/// Tier 1 (Cloudflare Workers) is heavily constrained on CPU time and memory;
/// limits are chosen to keep the full pipeline under ~15 ms on a Worker.
/// Tier 2 runs on a real server with no such budget, so limits can be relaxed
/// considerably.
///
/// Stored on [`crate::cook::Runtime`] so the tier 2 binary can swap in
/// [`ShortcutLimits::TIER2`] via [`crate::with_shortcut_limits`] at startup.
#[derive(Debug, Clone, Copy)]
pub struct ShortcutLimits {
    /// Maximum source pixel count for the progressive JPEG shortcut.
    ///
    /// JPEG decode cost is roughly linear with pixel count (~6–7 ms/MP on a
    /// Cloudflare Worker).  Tier 1 caps at 1 MP to stay under ~15 ms total.
    /// Tier 2 can raise this to handle large camera JPEGs directly.
    pub max_progressive_pixels: u64,

    /// Maximum file size for the "small image" shortcut (full in-memory decode).
    ///
    /// Files at or below this threshold are read whole and decoded without
    /// any range-request logic.  Tier 1 keeps this tight to avoid large
    /// Worker memory allocations.
    pub small_file_threshold: u64,

    /// Tail window fetched for the ZIP container shortcut.
    ///
    /// A single Range request for the last `zip_tail_size` bytes is expected
    /// to capture both the Central Directory and the embedded thumbnail.
    /// Larger values cover bigger office document thumbnails at the cost of
    /// fetching more data on cache miss.
    pub zip_tail_size: usize,
}

impl ShortcutLimits {
    /// Conservative limits for Tier 1 (Cloudflare Workers).
    ///
    /// Sized to keep the full CPU pipeline under ~15 ms and memory under
    /// the Worker heap limit.
    pub const TIER1: Self = Self {
        max_progressive_pixels: 1_000_000,       // ~1 MP — ~7 ms decode
        small_file_threshold:   80 * 1024,        // 80 KiB
        zip_tail_size:          128 * 1024,        // 128 KiB
    };

    /// Relaxed limits for Tier 2 (native server, no Worker budget).
    ///
    /// Progressive JPEG is effectively unbounded so tier 2 always attempts the
    /// partial-read shortcut for progressive sources before any full-file JPEG path.
    /// Small-file threshold is kept moderate for non-progressive inline decodes.
    /// ZIP tail is raised to 2 MiB to cover large office document previews.
    pub const TIER2: Self = Self {
        max_progressive_pixels: u64::MAX,
        small_file_threshold:   200 * 1024,       // 200 KiB
        zip_tail_size:          2 * 1024 * 1024,  // 2 MiB
    };
}
