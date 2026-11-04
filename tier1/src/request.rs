//! Call and thumb request types - the caller-facing input contract.
//!
//! A `CallRequest` is the outer HTTP request envelope carrying one or more
//! `ThumbInput` values.  `ThumbInput` normalises to a URL + options without
//! creating a separate intermediate struct - the pipeline (`ThumbPipeline`)
//! is constructed directly from the input.

use serde::{Deserialize, Serialize};

use crate::source::CacheHints;

//  Per-item input

/// Sentinel value accepted in the `cache` field meaning "do not cache".
///
/// A real cache token always has the shape `<hex_epoch>:<base64>`; this plain
/// word is visually distinct and can never be mistaken for a token.  When sent,
/// the server performs no cache reads and stores nothing for the request.
pub const NO_CACHE: &str = "uncached";

/// Parse a caller-supplied `cache` field into structured hints plus the
/// "do not cache" flag.
///
/// The [`NO_CACHE`] sentinel (matched case-insensitively, after trimming) is
/// recognised here so it never reaches [`CacheHints::decode`] or the cook's
/// `cache_hints`.  Empty, absent, and malformed values all decode to `None`
/// with caching left enabled.
pub fn decode_cache_field(value: Option<&str>) -> (Option<CacheHints>, bool) {
    match value.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case(NO_CACHE) => (None, true),
        Some(v) => (CacheHints::decode(v), false),
        None => (None, false),
    }
}

/// Accepts either a bare URL string or a full object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThumbInput {
    /// Bare URL string - all other fields take defaults.
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
    ///
    /// Send [`NO_CACHE`] (`"uncached"`) to bypass caching for this item: no
    /// cache reads, no conditional fetch, and no write of the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,

    /// Optional caller-supplied cache identity.
    ///
    /// When present this replaces the canonical URL as the cache lookup key
    /// for this item.  Two items with the same `url` but different `key`
    /// values are cached independently.  When absent the canonical form of
    /// `url` is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl ThumbInput {
    /// Extract the URL, cache hints, optional cache identity, and the
    /// "do not cache" flag without allocating an intermediate struct.
    ///
    /// Returns `(url, cache, key, cache_disabled)`.  An empty or whitespace-only
    /// `key` is normalised to `None`.  The `cache` field may be the
    /// [`NO_CACHE`] sentinel, in which case `cache` is `None` and
    /// `cache_disabled` is `true`.
    pub fn into_parts(self) -> (String, Option<CacheHints>, Option<String>, bool) {
        match self {
            Self::Url(url) => (url, None, None, false),
            Self::Object(obj) => {
                let (cache, cache_disabled) = decode_cache_field(obj.cache.as_deref());
                (
                    obj.url,
                    cache,
                    obj.key.as_deref().map(str::trim).filter(|k| !k.is_empty()).map(str::to_owned),
                    cache_disabled,
                )
            }
        }
    }
}

//  Call request

/// Top-level batch call request - the outer HTTP request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRequest {
    /// Items to process.
    pub items: Vec<ThumbInput>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(cache: Option<&str>) -> ThumbInput {
        ThumbInput::Object(ThumbObject {
            url: "https://example.com/a.png".to_string(),
            cache: cache.map(str::to_owned),
            key: None,
        })
    }

    #[test]
    fn uncached_sentinel_disables_cache_without_hints() {
        let (_, hints, _, no_cache) = object(Some(NO_CACHE)).into_parts();
        assert!(no_cache);
        assert!(hints.is_none());
    }

    #[test]
    fn uncached_sentinel_is_case_insensitive_and_trimmed() {
        for value in ["uncached", "  UNCACHED ", "Uncached"] {
            let (_, _, _, no_cache) = object(Some(value)).into_parts();
            assert!(no_cache, "value {value:?} should disable caching");
        }
    }

    #[test]
    fn missing_and_empty_cache_leave_caching_enabled() {
        for value in [None, Some(""), Some("   ")] {
            let (_, hints, _, no_cache) = object(value).into_parts();
            assert!(!no_cache, "value {value:?} should not disable caching");
            assert!(hints.is_none());
        }
    }

    #[test]
    fn real_token_still_decodes() {
        let hints = CacheHints::expiring_in(60);
        let token = hints.encode(60);
        let (_, decoded, _, no_cache) = object(Some(&token)).into_parts();
        assert!(!no_cache);
        assert!(decoded.unwrap().expires_at.is_some());
    }
}
