//! Batch request types — the caller-facing input contract.
//!
//! A `BatchRequest` carries one or more `ItemRequest` values.  Each item
//! identifies a source and declares which outputs are wanted.  Batch-level
//! options (cache mode, timeout, …) are on `BatchOptions`.

use serde::{Deserialize, Serialize};
use crate::source::SourceRef;

// ── Per-item input ────────────────────────────────────────────────────────────

/// Accepts either a bare URL string or a full object, making the simple case
/// ergonomic without sacrificing expressiveness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ItemInput {
    /// Bare URL string — all other fields take defaults.
    Url(String),
    /// Full item object.
    Object(ItemObject),
}

/// Full object-form item input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemObject {
    /// Opaque caller-supplied identifier echoed back on every event for this
    /// item.  Useful for correlating streaming events to the original request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Source URL.
    pub url: String,

    /// Caller's previously seen validator token from a prior response.
    ///
    /// When provided the service will issue a conditional fetch and can return
    /// `not_modified` instead of re-generating the thumbnail.
    ///
    /// Opaque encoding (prefix determines kind):
    /// - `E…` → sent as `If-None-Match`
    /// - `M…` → sent as `If-Modified-Since`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator: Option<String>,

    /// Which outputs to compute for this item.
    #[serde(default)]
    pub ops: RequestedOps,
}

/// A normalised, internal item request produced from `ItemInput`.
#[derive(Debug, Clone)]
pub struct ItemRequest {
    /// Caller-supplied id, if any.
    pub id: Option<String>,
    /// Resolved source reference.
    pub source: SourceRef,
    /// Caller's previously seen validator token.
    pub validator: Option<String>,
    /// Which outputs to produce.
    pub ops: RequestedOps,
}

impl ItemRequest {
    pub fn from_input(input: ItemInput) -> Self {
        match input {
            ItemInput::Url(url) => Self {
                id: None,
                source: SourceRef::url(url),
                validator: None,
                ops: RequestedOps::default(),
            },
            ItemInput::Object(obj) => Self {
                id: obj.id,
                source: SourceRef::url(obj.url),
                validator: obj.validator,
                ops: obj.ops,
            },
        }
    }
}

// ── Requested operations ──────────────────────────────────────────────────────

/// Which outputs the caller wants for a single item.
///
/// Both default to `true` so callers that omit this field get everything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestedOps {
    /// Return structured file description (file intelligence output).
    #[serde(default = "bool_true")]
    pub describe: bool,
    /// Return a thumbnail image.
    #[serde(default = "bool_true")]
    pub thumbnail: bool,
}

impl Default for RequestedOps {
    fn default() -> Self {
        Self { describe: true, thumbnail: true }
    }
}

fn bool_true() -> bool { true }

// ── Cache mode ────────────────────────────────────────────────────────────────

/// How the service should interact with the cache for this batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    /// Use the cache for reads and writes (default).
    #[default]
    ReadWrite,
    /// Read from cache but never write new entries.
    ReadOnly,
    /// Ignore the cache entirely; always regenerate and never persist.
    Disabled,
}

// ── Batch request ─────────────────────────────────────────────────────────────

/// Top-level batch request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequest {
    /// Items to process.
    pub items: Vec<ItemInput>,
    /// Batch-level options.
    #[serde(default)]
    pub options: BatchOptions,
}

/// Options that apply to the whole batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchOptions {
    /// Cache interaction mode.
    #[serde(default)]
    pub cache: CacheMode,
    /// Overall wall-clock timeout for the batch in seconds.
    /// `None` means the server's default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<f64>,
}
