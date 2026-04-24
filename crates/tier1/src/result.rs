//! Result types — the public per-item response contract.
//!
//! # Key types
//!
//! - [`JobStatus`] — high-level per-item outcome.
//! - [`ItemResponse`] — the public per-item response.  This is the cache
//!   object: stored on success and returned verbatim on a cache hit (with
//!   `status` overridden to `cached`).  All fields always serialise so
//!   clients get a stable JSON shape.  A thumbnail is always present —
//!   fallback placeholder images are used for error/throttle/not-modified.
//! - [`ServerInfo`] — internal per-item telemetry filled by the pipeline.
//!   Never sent to clients; used for logging.
//! - [`BatchResponse`] — top-level batch response wrapping a
//!   [`RequestRecord`] and a `Vec<ItemResponse>`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::media::FileKind;

// ── Job status ────────────────────────────────────────────────────────────────

/// High-level outcome of processing a single batch item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Thumbnail generated successfully this request.
    Success,
    /// Result returned from cache — no reprocessing was needed.
    Cached,
    /// Source unchanged since the caller's supplied validator token.
    NotModified,
    /// Processing failed; see the `message` field for details.
    Failed,
    /// Request deferred — caller is rate-limited or over quota.
    DeferUser,
    /// Request deferred — server is at capacity (shared resource limit).
    DeferServer,
}

// ── Request record ────────────────────────────────────────────────────────────

/// Per-HTTP-request tracking record.
///
/// One record per inbound HTTP request, linking all per-item `ServerInfo`
/// records produced during that request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub host: String,
    pub path: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Wall-clock seconds from request start to all items complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
}

// ── Server info ───────────────────────────────────────────────────────────────

/// Per-item server-side telemetry.
///
/// Included in the response when developer mode is enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// ID of the enclosing HTTP request.
    pub request_id: String,
    /// Canonical fetch URL (query params / auth tokens stripped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_url: Option<String>,
    /// Source byte length from `Content-Length`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_size: Option<u64>,
    /// MIME type (magic-sniffed preferred over `Content-Type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_mime: Option<String>,
    /// Validator token as returned by upstream (ETag or Last-Modified).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_validator: Option<String>,
    /// Whether the upstream server supports byte-range requests.
    pub fetch_ranges: bool,
    /// Total bytes received from the upstream source.
    pub download_bytes: u64,
    /// Extra bytes fetched in a tail Range read (e.g. TIFF IFD).
    pub download_tail: u64,
    /// Seconds spent waiting for upstream download(s).
    pub download_duration: f64,
    /// Seconds spent on decode / pre-processing (excluding final encode).
    pub render_duration: f64,
    /// Seconds spent on the crop / resize / mozjpeg encode step.
    pub encode_duration: f64,
    /// Pixel dimensions of the image entering the encode step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode_height: Option<u32>,
    /// Byte length of the encoded JPEG thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_bytes: Option<u64>,
    pub server_tier: u8,
}

// ── Item result ───────────────────────────────────────────────────────────────

/// Fully resolved result for one batch item.
///
/// Per-item response — the public API and cache object.
///
/// All fields are always present in the serialised JSON.  A thumbnail is
/// always provided — a pregenerated placeholder is used for error, throttle,
/// and not-modified outcomes so clients never need a nil check.
///
/// This struct is stored in the cache on `success` and returned verbatim
/// on a `cached` hit (with `status` overridden to `cached`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemResponse {
    /// Source URL — the client's correlation key.
    pub url: String,
    /// High-level processing outcome.
    pub status: JobStatus,
    /// Wall-clock seconds to generate this result.
    pub duration: f64,
    /// Bytes read from the source to generate this result.
    pub download_size: u64,
    /// Status message; empty on success, human-readable on failure/defer.
    pub message: String,
    /// Processing strategy: `render`, `progressive`, `embedded`, `fallback`.
    pub strategy: Option<String>,
    /// Freshness token for conditional re-requests; `null` if unavailable.
    pub etag: Option<String>,
    /// JPEG thumbnail bytes, base64-encoded.  Always present.
    #[serde(with = "base64_bytes")]
    pub thumbnail: Vec<u8>,
    /// Stable token identifying the placeholder image, when the thumbnail is a
    /// generic icon rather than a real render.  `null` for real thumbnails.
    ///
    /// Clients can use this as a cache key to share one image buffer across
    /// all items that map to the same placeholder instead of decoding the
    /// embedded bytes repeatedly.  Stable token examples:
    /// `"archive"`, `"error_404"`, `"error_auth"`, `"unsupported"`.
    pub placeholder: Option<String>,
    /// MIME type of the source (magic-sniffed preferred over Content-Type).
    pub mime: Option<String>,
    /// Content-Length of the source in bytes; `null` if not provided by server.
    pub file_size: Option<u64>,
    /// Coarse media category.
    pub kind: Option<FileKind>,
    /// Canonical file extension, no dot (e.g. `jpeg`, `png`, `pdf`).
    /// Enumerated — normalised form only, no aliases.
    pub extension: Option<String>,
    /// Format-specific properties; shape varies by `kind`.
    pub properties: Option<Value>,
}

impl Default for ItemResponse {
    fn default() -> Self {
        Self {
            url: String::new(),
            status: JobStatus::Failed,
            duration: 0.0,
            download_size: 0,
            message: String::new(),
            strategy: None,
            etag: None,
            thumbnail: Vec::new(),
            placeholder: None,
            mime: None,
            file_size: None,
            kind: None,
            extension: None,
            properties: None,
        }
    }
}

// ── Batch response ────────────────────────────────────────────────────────────

/// Top-level response body for the synchronous batch endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    pub request: RequestRecord,
    pub items: Vec<ItemResponse>,
}

// ── base64 serde helper ───────────────────────────────────────────────────────

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}
