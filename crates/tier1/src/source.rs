//! Source reference and transport metadata.
//!
//! A `SourceRef` is the caller's pointer to a piece of media.  Currently that
//! is always a URL; uploads and object-store references will be added later.
//!
//! `SourceMetadata` is what we learn about the source from HTTP headers and
//! magic-byte sniffing — before any heavy decode work begins.

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

// ── Source validator ──────────────────────────────────────────────────────────

/// An upstream freshness token that the caller can send back on subsequent
/// requests to get a `not_modified` response instead of a full regeneration.
///
/// Encoding is opaque to callers; the leading character encodes the kind:
/// - `E…` → ETag value
/// - `M…` → Last-Modified value
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceValidator(pub String);

impl SourceValidator {
    pub fn from_etag(value: &str) -> Self {
        Self(format!("E{value}"))
    }

    pub fn from_last_modified(value: &str) -> Self {
        Self(format!("M{value}"))
    }

    /// Return the raw validator string in the form expected by the corresponding
    /// HTTP conditional request header.
    pub fn header_value(&self) -> &str {
        self.0.get(1..).unwrap_or("")
    }

    /// `true` if this is an ETag-based validator.
    pub fn is_etag(&self) -> bool {
        self.0.starts_with('E')
    }
}

// ── HTTP-level source metadata ────────────────────────────────────────────────

/// Everything we learn about a source from headers and magic-byte sniffing,
/// before any heavy decode begins.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMetadata {
    /// MIME type from the `Content-Type` response header.
    pub content_type: Option<String>,
    /// MIME type from libmagic / `infer` byte sniffing.
    pub magic_mime: Option<String>,
    /// Byte length from `Content-Length` (may be absent for chunked responses).
    pub content_length: Option<u64>,
    /// Freshness token from `ETag` or `Last-Modified`, preferring ETag.
    pub validator: Option<SourceValidator>,
    /// `true` if the upstream server sent `Accept-Ranges: bytes`.
    pub accept_ranges: bool,
    /// Final URL after following any HTTP redirects.
    pub final_url: Option<String>,
}

impl SourceMetadata {
    /// Best-effort MIME: magic sniff wins over `Content-Type` header.
    pub fn best_mime(&self) -> Option<&str> {
        self.magic_mime
            .as_deref()
            .or(self.content_type.as_deref())
    }
}
