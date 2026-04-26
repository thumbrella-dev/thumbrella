//! Media metadata — file intelligence types.
//!
//! `FileKind` is the stable coarse classification returned to clients.
//! Format-specific properties are returned as an untyped `serde_json::Value`
//! map under the `properties` field of `ItemResult` — the shape varies by
//! kind and will be pinned once the inspect step is implemented.

use serde::{Deserialize, Serialize};

// ── File kind ─────────────────────────────────────────────────────────────────

/// Coarse category of a media source.
///
/// This is a product-visible enumeration; keep the variants stable and add new
/// ones conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    Unknown,
}

impl Default for FileKind {
    fn default() -> Self {
        Self::Unknown
    }
}

// ── Strategy ──────────────────────────────────────────────────────────────────

/// How the thumbnail was (or will be) produced.
///
/// Determines which pipeline branch runs and what the result looks like.
/// Returned in [`ThumbResult`](crate::result::ThumbResult) so clients can
/// reason about quality and provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Full decode → resize → encode.  Highest quality; requires pixel work.
    Render,
    /// Progressive/streaming decode that can emit a thumbnail before the full
    /// file is received (e.g. progressive JPEG, interlaced PNG).
    Progressive,
    /// Extract an existing thumbnail already embedded in the file
    /// (EXIF JPEG, HEIC cover image, DOCX/ODT preview, …).  No pixel work.
    Embedded,
    /// Source cannot be rendered; a pre-generated placeholder icon is used.
    Fallback,
}
