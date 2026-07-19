//! Media metadata - file intelligence types.
//!
//! `FileKind` is the stable coarse classification returned to clients.
//! Format-specific properties are returned as a `serde_json::Value` map
//! under `ThumbMedia.properties`.  Fields are shared across kinds where
//! they have the same meaning.  If a value cannot be determined reliably
//! it is omitted rather than guessed.
//!
//! ## Property schema by kind
//!
//! ```text
//! Image
//!   width     u32    Source pixel width (not thumbnail)
//!   height    u32    Source pixel height
//!   bpp       u32    Colour bits per pixel, no alpha.
//!                    Omit when ambiguous (DDS, JPEG without SOF marker).
//!   alpha     bool   Has transparency / alpha channel
//!   lossless  bool   True when the codec is losslessly compressed
//!
//! Animated images (GIF, APNG) are still classified as `Image`.  No
//! animation-specific fields - looping makes duration meaningless and
//! the `kind` already signals the possibility to clients.
//!
//! Video
//!   width     u32    Frame width
//!   height    u32    Frame height
//!   bpp       u32    If known from codec, else omit
//!   duration  f64    Seconds
//!   channels  u32    Audio track count, 0 if silent, omit if unknown
//!
//! Audio
//!   channels  u32    Audio channel count
//!   duration  f64    Seconds
//!   lossless  bool   Inferred from extension (flac, wav, aiff)
//!
//! Geometry, Vector, Document, Archive, Text, Binary, Unknown
//!   - no properties for now.  Counts (pages, vertices, files) and
//!     metadata (author, encoding) require deep format parsing or are
//!     unreliable from a header-only read.
//! ```

use serde::{Deserialize, Serialize};

//  File kind

/// Coarse category of a media source.
///
/// This is a product-visible enumeration; keep the variants stable and add new
/// ones conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum FileKind {
    /// Raster still pixel data.
    Image,
    /// Video with or without audio.
    Video,
    /// Audio only.
    Audio,
    /// Vector graphics (SVG, AI, EPS, …).
    Vector,
    /// Rich text and media document (PDF, DOCX, PPTX, ODT, …).
    Document,
    /// 3-D model or scene (USD, GLTF, OBJ, …).
    Geometry,
    /// Collection of files and directories (ZIP, TAR, RAR, …).
    Archive,
    /// Plain or marked up text.
    Text,
    /// Executable, font, or other binary not in the above categories.
    Binary,
    /// Could not be determined.
    #[default]
    Unknown,
}
