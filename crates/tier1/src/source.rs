//! Source reference and URL utilities.
//!
//! A `SourceRef` is the caller's pointer to a piece of media.  Currently that
//! is always a URL; uploads and object-store references will be added later.
//!
//! HTTP-level metadata (content type, content length, accept-ranges, …) is
//! read directly from `HttpBuffer` during the connect step.  The fields that
//! need to persist after the connection closes (`final_url`, the upstream etag)
//! are stored in `ThumbTrace`.

use serde::{Deserialize, Serialize};

// ── Source reference ──────────────────────────────────────────────────────────

/// A pointer to an input source.
///
/// Serialised with a `"type"` discriminant so JSON round-trips are stable even
/// as new variants are added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceRef {
    /// A remote resource addressed by URL.
    Url { url: String },
}

impl SourceRef {
    /// Convenience constructor.
    pub fn url(url: impl Into<String>) -> Self {
        Self::Url { url: url.into() }
    }

    /// The raw URL string if this is a URL variant.
    pub fn as_url(&self) -> Option<&str> {
        match self {
            Self::Url { url } => Some(url),
        }
    }
}

// ── Canonical URL ─────────────────────────────────────────────────────────────

/// Normalise a URL into a stable form suitable for use as a cache key.
///
/// Rules applied:
/// - Scheme and host are lowercased.
/// - All query parameters and fragment identifiers are stripped.
///   Storage services (S3, R2, GCS, …) embed signing tokens in query params;
///   the path alone identifies the resource.
/// - Non-HTTP schemes (`file://`, …) are returned unchanged.
///
/// Returns `None` only if the URL is structurally malformed (no `://`).
pub fn canonical_url(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Some(raw.to_owned());
    }
    // Strip fragment then query.
    let rest = rest.split('#').next().unwrap_or(rest);
    let rest = rest.split('?').next().unwrap_or(rest);
    // Lowercase the host (everything before the first '/').
    let (host, path) = if let Some(idx) = rest.find('/') {
        (&rest[..idx], &rest[idx..])
    } else {
        (rest, "")
    };
    Some(format!("{scheme}://{}{path}", host.to_ascii_lowercase()))
}

// ── Etag helpers ─────────────────────────────────────────────────────────────

/// Extract a freshness token from HTTP response headers.
///
/// Prefers `ETag` over `Last-Modified`.  The returned string is opaque — its
/// leading character encodes the kind so [`conditional_headers`] can reconstruct
/// the right request header without the caller needing to track that separately:
/// - `E…` → was an ETag
/// - `M…` → was a Last-Modified value
///
/// Returns `None` if neither header is present.
pub fn etag_from_headers(headers: &std::collections::HashMap<String, String>) -> Option<String> {
    if let Some(v) = headers.get("etag") {
        return Some(format!("E{v}"));
    }
    if let Some(v) = headers.get("last-modified") {
        return Some(format!("M{v}"));
    }
    None
}

/// Return the HTTP conditional-request headers for a stored etag string.
///
/// The inverse of [`etag_from_headers`]: given the opaque token produced
/// earlier, returns the `(header-name, value)` pair to include in a
/// subsequent fetch so the server can respond with `304 Not Modified`.
///
/// Returns `None` if the string is empty or has an unrecognised prefix.
pub fn conditional_headers(etag: &str) -> Option<(&'static str, &str)> {
    match etag.as_bytes().first() {
        Some(b'E') => Some(("if-none-match", &etag[1..])),
        Some(b'M') => Some(("if-modified-since", &etag[1..])),
        _ => None,
    }
}
