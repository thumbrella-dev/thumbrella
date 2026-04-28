//! Call and thumb response types — the public outbound contract.
//!
//! # Key types
//!
//! - [`JobStatus`] — high-level per-item outcome returned to the client.
//! - [`ThumbResult`] — the per-item result materialised from [`ThumbCook`] at
//!   the end of processing.  This is what gets serialised to the client, stored
//!   in cache, and returned verbatim on a cache hit.
//! - [`ThumbTrace`] — internal per-item telemetry materialised from [`ThumbCook`]
//!   and emitted to the configured log sink.  Never sent to clients.
//! - [`CallRecord`] / [`CallResponse`] — per-HTTP-request envelope types.
//!
//! Neither `ThumbResult` nor `ThumbTrace` exist during thumbnail processing —
//! they are output views constructed once at the end of [`ThumbCook::run`].

use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::cook::CallerContext;
use crate::media::{FileKind, Strategy};

// ── Cache outcome ─────────────────────────────────────────────────────────────

/// Which cache backend provided (or skipped) this result — internal detail.
///
/// Stored in [`ThumbTrace`]; never sent to clients verbatim.  The public-facing
/// field on [`ThumbResult`] collapses all hit variants to `"hit"` via
/// [`CacheOutcome::public_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CacheOutcome {
    #[default]
    Miss,
    Ignore,
    File,
    CfCache,
    CfKv,
    Redis,
    Sqlite,
}

impl CacheOutcome {
    pub fn is_hit(self) -> bool {
        matches!(self, Self::File | Self::CfCache | Self::CfKv | Self::Redis | Self::Sqlite)
    }

    pub fn public_label(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Miss   => "miss",
            _            => "hit",
        }
    }
}

// ── Render handler ────────────────────────────────────────────────────────────

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

// ── Job status ────────────────────────────────────────────────────────────────

/// High-level outcome of processing a single item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Success,
    Cached,
    NotModified,
    #[default]
    Failed,
    DeferUser,
    DeferServer,
    Rendering,
}

// ── Call record ───────────────────────────────────────────────────────────────

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

// ── ThumbResult ───────────────────────────────────────────────────────────────

/// Per-item result — the public API output, cache object, and client response.
///
/// Materialised from [`ThumbCook`] by [`ThumbCook::to_result`] at the end of
/// processing.  This struct is stored in cache on `success` and returned
/// verbatim on a `cached` hit (with `status` overridden to `cached`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbResult {
    pub url: String,
    pub status: JobStatus,
    pub duration: f64,
    pub download_size: u64,
    /// Human-readable error/status detail; `None` on clean success.
    pub message: Option<String>,
    pub strategy: Option<Strategy>,
    pub etag: Option<String>,
    #[serde(with = "base64_bytes")]
    pub thumbnail: Vec<u8>,
    pub placeholder: Option<String>,
    pub mime: Option<String>,
    pub file_size: Option<u64>,
    pub kind: Option<FileKind>,
    pub extension: Option<String>,
    /// Format-specific properties (dimensions, color depth, …).  Always present;
    /// empty object `{}` when no properties were extracted.
    pub properties: Value,
    pub cache: Option<String>,
}

impl Default for ThumbResult {
    fn default() -> Self {
        Self {
            url:           String::new(),
            status:        JobStatus::Failed,
            duration:      0.0,
            download_size: 0,
            message:       None,
            strategy:      None,
            etag:          None,
            thumbnail:     Vec::new(),
            placeholder:   None,
            mime:          None,
            file_size:     None,
            kind:          None,
            extension:     None,
            properties:    Value::Object(Default::default()),
            cache:         None,
        }
    }
}

// ── ThumbTrace ────────────────────────────────────────────────────────────────

/// Internal per-item telemetry — the server's private record of work done.
///
/// Materialised from [`ThumbCook`] by [`ThumbCook::to_trace`] at the end of
/// processing.  Never sent to clients.  Written to the configured log sink.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ThumbTrace {
    // ── Request identity ──────────────────────────────────────────────────────
    /// RFC 3339 timestamp of when the trace was materialised.
    pub timestamp:    String,
    /// Outcome of the job, mirroring [`ThumbResult::status`].
    pub status:       JobStatus,
    /// Rendering strategy used (mirrors [`ThumbResult::strategy`]).
    pub strategy:     Option<Strategy>,
    /// Media kind detected (mirrors [`ThumbResult::kind`]).
    pub kind:         Option<FileKind>,
    /// File extension detected (mirrors [`ThumbResult::extension`]).
    pub extension:    Option<String>,

    // ── Source identity ───────────────────────────────────────────────────────
    pub canonical_url:    Option<String>,
    pub final_url:        Option<String>,
    pub cache_key:        Option<String>,
    pub cache_key_source: Option<String>,
    pub source_etag:      Option<String>,

    // ── Download metrics ──────────────────────────────────────────────────────
    pub download_bytes:      u64,
    pub download_tail_bytes: u64,
    pub connect_secs:        f64,

    // ── Step timing ───────────────────────────────────────────────────────────
    pub inspect_secs:   f64,
    pub shortcut_secs:  f64,
    pub render_secs:    f64,
    pub deliver_secs:   f64,

    // ── Render details ────────────────────────────────────────────────────────
    pub render_resolution: Option<[u32; 2]>,
    pub thumbnail_bytes:   Option<u64>,

    // ── Job provenance ────────────────────────────────────────────────────────
    pub job_tier:     u8,
    pub job_renderer: Option<String>,
    pub job_codec:    Option<String>,
    pub video_seek_secs: Option<f64>,

    // ── Attribution ───────────────────────────────────────────────────────────
    pub session_id:  Option<String>,
    pub customer_id: Option<String>,
    pub cache_hit:   Option<CacheOutcome>,
    pub render_handler: RenderHandler,
    pub caller:      Option<CallerContext>,
    pub cancelled:   bool,
    pub server:      Option<String>,
    pub version:     String,
}

// ── Call response ─────────────────────────────────────────────────────────────

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
    where S: Serializer {
        serializer.serialize_str(&STANDARD.encode(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where D: Deserializer<'de> {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}
