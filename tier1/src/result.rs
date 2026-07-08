//! Call and thumb response types — the public outbound contract.
//!
//! # Key types
//!
//! - [`ResultStatus`] — high-level per-item outcome returned to the client.
//! - [`ThumbResult`] — the per-item result materialised from
//!   [`crate::cook::ThumbCook`] at the end of processing.  This is what gets
//!   serialised to the client, stored in cache, and returned verbatim on a
//!   cache hit.
//! - [`ThumbTrace`] — internal per-item telemetry materialised from
//!   [`crate::cook::ThumbCook`] and emitted to the configured log sink.  Never
//!   sent to clients.
//! - [`CallRecord`] / [`CallResponse`] — per-HTTP-request envelope types.
//!
//! Neither `ThumbResult` nor `ThumbTrace` exist during thumbnail processing —
//! they are output views constructed once at the end of
//! [`crate::cook::ThumbCook::run`].

use crate::cook::CallerContext;
use crate::media::FileKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;

//  Render handler 

/// Which renderer handled (or attempted to handle) this item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RenderHandler {
    #[default]
    None,
    Builtin,
    Handoff,
    Fumble,
    Punt,
}

//  Job status 

/// High-level outcome of processing a single item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Success,
    #[default]
    Failed,
    /// Server is at capacity; client should retry later.
    Overloaded,
    Intermediate,
}

//  Source 

/// How the thumbnail was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultSource {
    /// Full compute render for this media type.
    Render,
    /// Embedded thumbnail extracted without a full render.
    Shortcut,
    /// Served from server-side cache.
    Cache,
    /// Client cache hints were valid; upstream resource unchanged.
    /// `media.thumbnail` is empty — the client should use its cached copy.
    NotModified,
    /// A registered renderer tried but could not handle this format.
    Fallback,
    /// No renderer was registered for this format at all.
    Placeholder,
    /// Not used by server, but defined and reserved for client handling.
    Client,
}

//  Call record

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub id: String,
    pub host: String,
    pub path: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
}

//  ThumbMedia 

/// The stable, cacheable unit of a thumbnail response.
///
/// Two results for the same source file share identical `ThumbMedia`.
/// Clients can compare fields to deduplicate across requests; the server
/// serialises this struct verbatim into its cache backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbMedia {
    /// `Content-Length` from the upstream server, or 0.
    pub file_size: u64,
    /// Detected media category.
    pub kind: FileKind,
    /// Canonical file extension, no dot (e.g. `"jpeg"`, `"png"`).
    pub extension: String,
    /// Sniffed MIME type (e.g. `"image/jpeg"`).
    pub mime: String,
    /// Cache token for round-tripping.  Format: `<hex_epoch>:<base64_blob>`.
    /// Clients check freshness against the epoch. Empty = do not cache.
    pub cache: String,
    /// Fallback icon token.  Non-empty when this is a placeholder result.
    /// Clients can compare this to deduplicate placeholder images.
    pub placeholder: String,
    /// Format-specific metadata (dimensions, colour depth, …).
    pub properties: Value,
    /// Encoded JPEG thumbnail bytes, base64 in JSON.
    #[serde(with = "base64_bytes")]
    pub thumbnail: Vec<u8>,
    /// Source URL that produced this thumbnail.
    pub url: String,
}

impl Default for ThumbMedia {
    fn default() -> Self {
        Self {
            url: String::new(),
            thumbnail: Vec::new(),
            mime: String::new(),
            cache: String::new(),
            placeholder: String::new(),
            file_size: 0,
            kind: FileKind::Unknown,
            extension: String::new(),
            properties: Value::Object(Default::default()),
        }
    }
}

//  ThumbResult

/// Per-request result: the public API response for one URL.
///
/// Top-level fields describe this invocation (status, timing, source).
/// [`media`](ThumbMedia) is the stable, cacheable payload — two results
/// for the same file share the same `media`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbResult {
    /// The source URL that was requested.
    pub url: String,
    /// High-level outcome.
    pub status: ResultStatus,
    /// Error or status detail; `None` on clean success.
    pub message: Option<String>,
    /// How the thumbnail was produced (render, shortcut, cache, …).
    #[serde(default)]
    pub source: Option<ResultSource>,
    /// Wall-clock seconds to produce this result.
    pub duration: f64,
    /// Bytes fetched from the upstream source.
    pub download_size: u64,
    /// HTTP status returned by the upstream source, if fetched.
    #[serde(default)]
    pub http_status: Option<u16>,
    /// The thumbnail and its metadata.  `None` on total failure.
    #[serde(default)]
    pub media: Option<ThumbMedia>,
}

impl Default for ThumbResult {
    fn default() -> Self {
        Self {
            url: String::new(),
            status: ResultStatus::Failed,
            message: None,
            source: None,
            duration: 0.0,
            download_size: 0,
            http_status: None,
            media: None,
        }
    }
}

//  ThumbTrace 

/// Internal per-item telemetry — the server's private record of work done.
///
/// Materialised from [`crate::cook::ThumbCook`] by
/// [`crate::cook::ThumbCook::to_trace`] at the end of processing.  Never sent
/// to clients.  Written to the configured log sink.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ThumbTrace {
    //  Request identity 
    /// RFC 3339 timestamp of when the trace was materialised.
    pub timestamp: String,
    /// Outcome of the job, mirroring [`ThumbResult::status`].
    pub status: ResultStatus,
    /// Media kind detected (mirrors [`ThumbMedia::kind`]).
    pub kind: Option<FileKind>,
    /// File extension detected (mirrors [`ThumbMedia::extension`]).
    pub extension: Option<String>,

    //  Source identity
    pub canonical_url: Option<String>,
    pub cache_key: Option<String>,
    pub cache_key_source: Option<String>,
    pub source_etag: Option<String>,
    //  Download metrics 
    pub download_bytes: u64,
    pub download_tail_bytes: u64,
    /// All time awaiting fetch (connect + transfer).
    pub io_secs: f64,

    //  Step timing
    /// Inspect phase, plus shortcut phase if it failed.
    pub inspect_secs: f64,
    /// Decode/render phase, or the shortcut phase when shortcut succeeded.
    pub render_secs: f64,
    pub deliver_secs: f64,

    //  Render details 
    pub thumbnail_bytes: Option<u64>,

    //  Job provenance 
    pub job_tier: u8,
    pub job_renderer: Option<String>,

    //  Failure detail 
    /// Human-readable error description; `None` on success.  Mirrors
    /// [`ThumbResult::message`] so the trace contains the full failure reason.
    pub message: Option<String>,

    //  Attribution
    pub session_id: Option<String>,
    pub customer_id: Option<String>,
    /// Name of the cache backend that produced the hit (e.g. `"sqlite"`, `"redis"`); `None` on miss.
    pub cache_hit: Option<String>,
    pub render_handler: RenderHandler,
    pub caller: Option<CallerContext>,
    pub cancelled: bool,
    pub server: Option<String>,
    pub version: String,
}

//  Call response

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallResponse {
    pub request: CallRecord,
    pub items: Vec<ThumbResult>,
}

//  base64 serde helper

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
