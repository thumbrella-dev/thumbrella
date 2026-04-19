//! Batch request types — the common input shape for all tiers.

use serde::{Deserialize, Serialize};
use crate::source::SourceRef;

/// A single item in request JSON. Accepts either a bare URL string or an
/// object with URL plus optional metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ItemInput {
    Url(String),
    UrlWithMeta(ItemInputObject),
}

/// Expanded object-form item input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemInputObject {
    /// Opaque caller-supplied identifier returned on every event for this item.
    pub id: Option<String>,
    /// Source URL.
    pub url: String,
    /// Caller's previously seen ETag.
    pub etag: Option<String>,
    /// What operations to perform for this item.
    #[serde(default)]
    pub ops: RequestedOps,
}

/// A single item within a batch request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemRequest {
    /// Opaque caller-supplied identifier returned on every event for this item.
    pub id: Option<String>,
    /// The source to inspect or thumbnail.
    pub source: SourceRef,
    /// Caller's previously seen ETag for this source. When provided, the
    /// service can short-circuit to `not_modified` without a full fetch.
    pub etag: Option<String>,
    /// What operations to perform for this item.
    #[serde(default)]
    pub ops: RequestedOps,
}

/// Which outputs the caller wants for an item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestedOps {
    /// Return structured file description.
    #[serde(default = "default_true")]
    pub describe: bool,
    /// Return a thumbnail image.
    #[serde(default = "default_true")]
    pub thumbnail: bool,
}

fn default_true() -> bool { true }

/// Top-level batch request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequest {
    /// Items to process.
    ///
    /// Accepts either:
    /// - bare URL strings, or
    /// - objects with `url` and optional `etag`, `id`, and `ops`.
    pub items: Vec<ItemInput>,
}

impl BatchRequest {
    /// Normalize external request items into the single internal item type.
    pub fn into_items(self) -> Vec<ItemRequest> {
        self.items
            .into_iter()
            .map(|item| match item {
                ItemInput::Url(url) => ItemRequest {
                    id: None,
                    source: SourceRef::Url { url },
                    etag: None,
                    ops: RequestedOps::default(),
                },
                ItemInput::UrlWithMeta(v) => ItemRequest {
                    id: v.id,
                    source: SourceRef::Url { url: v.url },
                    etag: v.etag,
                    ops: v.ops,
                },
            })
            .collect()
    }
}
