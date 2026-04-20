//! Media metadata — structured information extracted during decode and processing.
//!
//! This is the common data contract between Tier 1 and Tier 2, as well as the
//! information returned to clients. It captures:
//! - What decode strategy was used and why
//! - Measurements: bytes in/out, wall time, CPU effort
//! - Media properties: dimensions, color space, duration, etc.
//! - Warnings: problematic conditions that didn't block thumbnail generation

use serde::{Deserialize, Serialize};

/// The high-level strategy used to decode and generate a thumbnail.
///
/// This helps clients understand performance characteristics and informs
/// future optimization decisions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DecodeStrategy {
    /// Tier1: Fast path — JPEG progressive partial read
    JpegProgressive,
    /// Tier1: Fast path — PNG with interlace support
    PngInterlaced,
    /// Tier1: Fast path — TIFF embedded JPEG (JPEGInterchangeFormat + Compression=6/7)
    TiffEmbeddedJpeg,
    /// Tier1: Full image decode (most formats)
    FullImage,
    /// Tier1: ZIP/DOCX/ODT internal thumbnail or first image
    ContainerInternal,
    /// Tier2: libav video decode + frame selection (keyframe seek + scoring)
    Tier2LibavVideo,
    /// Tier2: libav HEIC/HEIF decode
    Tier2LibavHeic,
    /// Tier2: libav AVIF decode
    Tier2LibavAvif,
    /// Tier2: libav EXR decode + linear-to-sRGB tone mapping
    Tier2LibavExr,
    /// Tier2: Music cover art extraction (MP3 attached_pic, etc.)
    Tier2AttachedPicture,
    /// Not a format we can thumbnail
    Unsupported,
}

/// Color space / transfer function information extracted from the source.
///
/// Used to guide tone mapping (e.g., linear→sRGB for EXR).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorSpace {
    /// Standard sRGB (typical for most images and web)
    Srgb,
    /// Linear (unbounded) — typically needs tone mapping for display
    Linear,
    /// Display P3 color gamut
    DisplayP3,
    /// Rec. 709 (HD video)
    Rec709,
    /// Rec. 2020 (UHD video)
    Rec2020,
    /// Unknown or unspecified
    #[default]
    Unknown,
}

/// Media properties extracted during decode.
///
/// For images: dimensions of the original source.
/// For video: dimensions of the first decoded frame, plus duration.
/// For audio: no visual properties, but duration if available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaProperties {
    /// Width in pixels (original source, before any thumbnail resize)
    pub width: Option<u32>,
    /// Height in pixels (original source)
    pub height: Option<u32>,
    /// Duration in seconds (video and audio)
    pub duration_secs: Option<f64>,
    /// Detected color space / transfer function
    #[serde(default)]
    pub color_space: ColorSpace,
    /// Whether the image/video is interlaced or progressive
    pub interlaced: Option<bool>,
    /// Thumbnail dimensions (what was actually output)
    pub thumbnail_width: Option<u32>,
    pub thumbnail_height: Option<u32>,
}

impl Default for MediaProperties {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            duration_secs: None,
            color_space: ColorSpace::Unknown,
            interlaced: None,
            thumbnail_width: None,
            thumbnail_height: None,
        }
    }
}

/// Measurements recorded during processing.
///
/// Helps identify performance bottlenecks and validate optimization work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessingMetrics {
    /// Bytes downloaded/read from the source
    pub bytes_in: u64,
    /// JPEG thumbnail bytes output
    pub jpeg_bytes_out: u64,
    /// Wall time (seconds) to fetch and process
    pub wall_time_secs: f64,
    /// Number of HTTP requests made (for range fetches, etc.)
    pub http_requests: u32,
    /// Approximate seek offset for video (seconds)
    pub video_seek_offset_secs: Option<f64>,
}

impl Default for ProcessingMetrics {
    fn default() -> Self {
        Self {
            bytes_in: 0,
            jpeg_bytes_out: 0,
            wall_time_secs: 0.0,
            http_requests: 0,
            video_seek_offset_secs: None,
        }
    }
}

/// Non-fatal issues encountered during processing.
///
/// These don't prevent thumbnail generation but should be logged and
/// potentially reported back to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warning {
    /// Source color space was linear but no tone mapping was applied
    LinearWithoutToneMapping,
    /// Source has embedded preview but it's lower quality than expected
    DegradedEmbeddedPreview,
    /// TIFF/RAW file has no embedded JPEG preview — full decode was slow
    RawNoEmbeddedPreview,
    /// Video frame selection fell back to first frame (no motion/edges found)
    VideoFrameSelectionFallback,
    /// HTTP range request not supported; full download was necessary
    HttpRangeUnsupported,
    /// Custom message for unforeseen issues
    Custom(String),
}

/// Complete decoded media metadata — the contract between Tier 1 and Tier 2,
/// and what gets returned to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMetadata {
    /// Which decode strategy was used
    pub strategy: DecodeStrategy,
    /// Media properties (dimensions, duration, color space, etc.)
    pub properties: MediaProperties,
    /// Processing measurements
    pub metrics: ProcessingMetrics,
    /// Non-fatal warnings
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl Default for MediaMetadata {
    fn default() -> Self {
        Self {
            strategy: DecodeStrategy::Unsupported,
            properties: MediaProperties::default(),
            metrics: ProcessingMetrics::default(),
            warnings: Vec::new(),
        }
    }
}

impl MediaMetadata {
    /// Create a new metadata object with the given decode strategy.
    pub fn new(strategy: DecodeStrategy) -> Self {
        Self {
            strategy,
            ..Default::default()
        }
    }

    /// Add a warning.
    pub fn warn(&mut self, warning: Warning) {
        self.warnings.push(warning);
    }

    /// Conversion ratio: bytes_in to jpeg_bytes_out.
    pub fn compression_ratio(&self) -> f64 {
        if self.metrics.bytes_in == 0 {
            0.0
        } else {
            self.metrics.jpeg_bytes_out as f64 / self.metrics.bytes_in as f64
        }
    }

    /// Throughput: bytes_in per second.
    pub fn throughput_mbps(&self) -> f64 {
        if self.metrics.wall_time_secs <= 0.0 {
            0.0
        } else {
            (self.metrics.bytes_in as f64 / 1_000_000.0) / self.metrics.wall_time_secs
        }
    }
}

// ---------------------------------------------------------------------------
// File kind — canonical type identity derived from magic bytes or Content-Type
// ---------------------------------------------------------------------------

/// High-level category for media content.
///
/// Matches the `media_type` field in the API schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Image,
    Video,
    Audio,
    Document,
    Vector,
    Archive,
    Binary,
}

/// Canonical type identity for a file.
///
/// Derived from magic bytes (preferred) or Content-Type header (fallback).
/// The extension is normalised: 4-letter variant is used where it is the
/// primary IANA name (e.g. `jpeg` not `jpg`, `tiff` not `tif`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileKind {
    /// High-level category.
    pub media_type: MediaType,
    /// Canonical extension without a leading dot.
    /// Examples: `jpeg`, `png`, `webp`, `avif`, `mp4`, `pdf`, `zip`.
    pub extension: String,
    /// Which processing tier is expected to generate a thumbnail for this kind.
    pub thumbnail_tier: ThumbnailTier,
}

impl FileKind {
    fn new(media_type: MediaType, extension: &'static str) -> Self {
        let thumbnail_tier = Self::tier_for(extension);
        Self { media_type, extension: extension.to_string(), thumbnail_tier }
    }

    /// Which processing tier is expected to produce a thumbnail for this kind.
    pub fn thumbnail_tier(&self) -> ThumbnailTier {
        self.thumbnail_tier
    }

    fn tier_for(extension: &str) -> ThumbnailTier {
        match extension {
            // Tier 1: decoded by the `image` crate
            "jpeg" | "png" | "webp" | "gif" | "bmp" | "tiff" | "ico" | "apng"
            // Tier 1: office containers with embedded previews
            | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp"
            // Tier 1: SVG (rasterised via resvg or similar)
            | "svg" => ThumbnailTier::Tier1,
            // Tier 2: require libav or specialised decode
            "avif" | "heic" | "heif" | "exr"
            | "mp4" | "webm" | "mpeg" | "avi" | "mov" | "mkv" | "ts" | "ogv"
            | "mp3" | "aac" | "ogg" | "oga" | "opus" | "wav" | "flac" | "m4a" | "weba"
            | "3gpp" | "3gp2" => ThumbnailTier::Tier2,
            _ => ThumbnailTier::Unsupported,
        }
    }
}

/// Which processing tier is expected to handle thumbnail generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThumbnailTier {
    /// Tier 1 can handle this in-process.
    Tier1,
    /// Requires Tier 2 (video decode, HEIC, AVIF, EXR via libav, etc.).
    Tier2,
    /// No known handler — a fallback icon will be used.
    Unsupported,
}

/// Sniff the canonical file kind from magic bytes, with Content-Type as
/// fallback.
///
/// Magic bytes are preferred because Content-Type headers can be wrong or
/// absent. Returns `None` when the type cannot be determined.
pub fn sniff_file_kind(bytes: &[u8], content_type: Option<&str>) -> Option<FileKind> {
    // Magic bytes first — more reliable than server-supplied headers.
    if let Some(inferred) = infer::get(bytes) {
        if let Some(kind) = file_kind_from_mime(inferred.mime_type()) {
            return Some(kind);
        }
    }
    // Fall back to Content-Type.
    content_type.and_then(|ct| {
        let base = ct.split(';').next().unwrap_or(ct).trim();
        file_kind_from_mime(base)
    })
}

/// Map a MIME type string to a canonical `FileKind`.
fn file_kind_from_mime(mime: &str) -> Option<FileKind> {
    Some(match mime.trim().to_ascii_lowercase().as_str() {
        // --- Images — Tier 1 (decodable by the `image` crate) ---
        "image/jpeg"                    => FileKind::new(MediaType::Image, "jpeg"),
        "image/png"                     => FileKind::new(MediaType::Image, "png"),
        "image/webp"                    => FileKind::new(MediaType::Image, "webp"),
        "image/gif"                     => FileKind::new(MediaType::Image, "gif"),
        "image/bmp"                     => FileKind::new(MediaType::Image, "bmp"),
        "image/tiff"                    => FileKind::new(MediaType::Image, "tiff"),
        "image/vnd.microsoft.icon" | "image/x-icon"
                                        => FileKind::new(MediaType::Image, "ico"),
        "image/apng"                    => FileKind::new(MediaType::Image, "apng"),
        // Images — Tier 2
        "image/avif"                    => FileKind::new(MediaType::Image, "avif"),
        "image/heic" | "image/heif"     => FileKind::new(MediaType::Image, "heic"),
        "image/x-exr" | "image/openexr" | "image/x-openexr"
                                        => FileKind::new(MediaType::Image, "exr"),
        // Vector
        "image/svg+xml"                 => FileKind::new(MediaType::Vector, "svg"),
        // --- Video ---
        "video/mp4"                     => FileKind::new(MediaType::Video, "mp4"),
        "video/webm"                    => FileKind::new(MediaType::Video, "webm"),
        "video/mpeg"                    => FileKind::new(MediaType::Video, "mpeg"),
        "video/x-msvideo"               => FileKind::new(MediaType::Video, "avi"),
        "video/quicktime"               => FileKind::new(MediaType::Video, "mov"),
        "video/x-matroska"              => FileKind::new(MediaType::Video, "mkv"),
        "video/mp2t"                    => FileKind::new(MediaType::Video, "ts"),
        "video/ogg"                     => FileKind::new(MediaType::Video, "ogv"),
        "video/3gpp"                    => FileKind::new(MediaType::Video, "3gpp"),
        "video/3gpp2"                   => FileKind::new(MediaType::Video, "3gp2"),
        // --- Audio ---
        "audio/mpeg"                    => FileKind::new(MediaType::Audio, "mp3"),
        "audio/aac"                     => FileKind::new(MediaType::Audio, "aac"),
        "audio/ogg"                     => FileKind::new(MediaType::Audio, "ogg"),
        "audio/opus"                    => FileKind::new(MediaType::Audio, "opus"),
        "audio/wav" | "audio/x-wav"     => FileKind::new(MediaType::Audio, "wav"),
        "audio/flac"                    => FileKind::new(MediaType::Audio, "flac"),
        "audio/webm"                    => FileKind::new(MediaType::Audio, "weba"),
        "audio/mp4" | "audio/x-m4a"    => FileKind::new(MediaType::Audio, "m4a"),
        // --- Documents ---
        "application/pdf"               => FileKind::new(MediaType::Document, "pdf"),
        "application/msword"            => FileKind::new(MediaType::Document, "doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                                        => FileKind::new(MediaType::Document, "docx"),
        "application/vnd.ms-excel"      => FileKind::new(MediaType::Document, "xls"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                                        => FileKind::new(MediaType::Document, "xlsx"),
        "application/vnd.ms-powerpoint" => FileKind::new(MediaType::Document, "ppt"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                                        => FileKind::new(MediaType::Document, "pptx"),
        "application/vnd.oasis.opendocument.text"
                                        => FileKind::new(MediaType::Document, "odt"),
        "application/vnd.oasis.opendocument.spreadsheet"
                                        => FileKind::new(MediaType::Document, "ods"),
        "application/vnd.oasis.opendocument.presentation"
                                        => FileKind::new(MediaType::Document, "odp"),
        "text/html"                     => FileKind::new(MediaType::Document, "html"),
        "text/markdown"                 => FileKind::new(MediaType::Document, "md"),
        "text/plain"                    => FileKind::new(MediaType::Document, "txt"),
        "text/csv"                      => FileKind::new(MediaType::Document, "csv"),
        // --- Archives ---
        "application/zip" | "application/x-zip-compressed"
                                        => FileKind::new(MediaType::Archive, "zip"),
        "application/x-tar"             => FileKind::new(MediaType::Archive, "tar"),
        "application/gzip" | "application/x-gzip"
                                        => FileKind::new(MediaType::Archive, "gz"),
        "application/x-bzip2"           => FileKind::new(MediaType::Archive, "bz2"),
        "application/x-7z-compressed"   => FileKind::new(MediaType::Archive, "7zip"),
        "application/vnd.rar" | "application/x-rar-compressed"
                                        => FileKind::new(MediaType::Archive, "rar"),
        // Generic binary (known unknown)
        "application/octet-stream"      => FileKind::new(MediaType::Binary, "bin"),
        _ => return None,
    })
}

