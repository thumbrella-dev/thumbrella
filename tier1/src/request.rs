//! Call and thumb request types — the caller-facing input contract.
//!
//! A `CallRequest` is the outer HTTP request envelope carrying one or more
//! `ThumbInput` values.  `ThumbInput` normalises to a URL + options without
//! creating a separate intermediate struct — the pipeline (`ThumbPipeline`)
//! is constructed directly from the input.

use serde::{Deserialize, Serialize};

use crate::source::CacheHints;

//  Per-item input 

/// Accepts either a bare URL string or a full object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThumbInput {
    /// Bare URL string — all other fields take defaults.
    Url(String),
    /// Full item object.
    Object(ThumbObject),
}

/// Full object-form item input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbObject {
    /// Source URL.
    pub url: String,

    /// Opaque cache token from a prior `ThumbResult.cache`.
    ///
    /// Round-trip this value unchanged.  The server uses it for conditional
    /// fetches and client-side freshness checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
}

impl ThumbInput {
    /// Extract the URL and cache hints without allocating an intermediate struct.
    pub fn into_parts(self) -> (String, Option<CacheHints>) {
        match self {
            Self::Url(url) => (url, None),
            Self::Object(obj) => (obj.url, obj.cache.as_deref().and_then(CacheHints::decode)),
        }
    }
}

//  Call request 

/// Top-level batch call request — the outer HTTP request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRequest {
    /// Items to process.
    pub items: Vec<ThumbInput>,
}
