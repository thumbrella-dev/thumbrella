//! Call and thumb response types — the public outbound contract.
//!
//! # Key types
//!
//! - [`JobStatus`] — high-level per-item outcome.
//! - [`ThumbResult`] — the per-item result.  Public API, cache object, and
//!   cook output are all this same struct.  All fields always serialise so
//!   clients get a stable JSON shape.  A thumbnail is always present except
//!   when `status` is `not_modified` — the only case where `thumbnail` is empty.
//! - [`CallRecord`] — per-HTTP-request envelope record (id, host, path, …).
//! - [`CallResponse`] — top-level response wrapping one `CallRecord` and
//!   a `Vec<ThumbResult>`.
//!
//! Internal pipeline telemetry lives in `cook::ThumbTrace` — not here.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::media::{FileKind, Strategy};

// ── Job status ────────────────────────────────────────────────────────────────

/// High-level outcome of processing a single item.
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
    /// Thumbnail is being rendered by a higher-tier worker; a placeholder is
    /// present now and a final result will follow on streaming endpoints.
    ///
    /// Only emitted on `POST /stream`.  Clients that do not handle streaming
    /// (e.g. callers of `POST /batch`) will never see this status.
    Rendering,
}

// ── Call record ───────────────────────────────────────────────────────────────

/// Per-HTTP-request tracking record — the envelope for a batch call.
///
/// One record per inbound HTTP request, correlating all `ThumbResult`
/// items produced during that call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
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

// ── Thumb result ─────────────────────────────────────────────────────────────

/// Per-item result — the public API, cache object, and cook output.
///
/// All fields are always present in the serialised JSON.  A thumbnail is
/// always provided — a pregenerated placeholder is used for error, throttle,
/// and defer outcomes so clients never need a nil check.  The one exception
/// is `status: not_modified`: when the caller supplied an `etag` and the
/// source is unchanged, `thumbnail` is empty and no rendering was done.
///
/// This struct is stored in the cache on `success` and returned verbatim
/// on a `cached` hit (with `status` overridden to `cached`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbResult {
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
    /// Processing strategy used to produce the thumbnail.
    pub strategy: Option<Strategy>,
    /// Freshness token for conditional re-requests; `null` if unavailable.
    pub etag: Option<String>,
    /// JPEG thumbnail bytes, base64-encoded.  Always present.
    #[serde(with = "base64_bytes")]
    pub thumbnail: Vec<u8>,
    /// Stable token identifying the placeholder image, when the thumbnail is a
    /// generic icon rather than a real render.  `null` for real thumbnails.
    ///
    /// Clients can use this as a cache key to share one image buffer across
    /// all items that map to the same placeholder.  Example tokens:
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

impl Default for ThumbResult {
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

// ── Call response ─────────────────────────────────────────────────────────────

/// Top-level response body for a batch call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallResponse {
    pub request: CallRecord,
    pub items: Vec<ThumbResult>,
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
