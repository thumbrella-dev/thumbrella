//! Source reference and transport metadata.

use serde::{Deserialize, Serialize};
use crate::media::FileKind;

/// Normalise a URL into a stable canonical form suitable for use as a cache key.
///
/// Rules applied:
/// - Scheme and host are lowercased.
/// - All query parameters and fragment identifiers are stripped.
///   For storage services (S3, R2, GCS, Spaces, …) the signing/auth query
///   parameters are always noise — the path alone identifies the resource.
///   For S3-versioned objects the `versionId` is part of the path key, not
///   the query string, so stripping queries is safe there too.
/// - `file://` and other non-HTTP schemes are returned unchanged.
///
/// Returns `None` only if the URL is malformed (missing `://`).
pub fn canonical_url_for(raw_url: &str) -> Option<String> {
    let (scheme, rest) = raw_url.split_once("://")?;
    let scheme_lower = scheme.to_ascii_lowercase();
    if scheme_lower != "http" && scheme_lower != "https" {
        // file:// and others carry no auth query params — return as-is.
        return Some(raw_url.to_string());
    }
    // Strip fragment, then query.
    let rest = rest.split('#').next().unwrap_or(rest);
    let rest = rest.split('?').next().unwrap_or(rest);
    // Normalise host (everything before the first '/') to lowercase.
    let (host, path) = if let Some(idx) = rest.find('/') {
        (&rest[..idx], &rest[idx..])
    } else {
        (rest, "")
    };
    Some(format!("{scheme_lower}://{}{path}", host.to_ascii_lowercase()))
}

/// A reference to an input source. Currently URL-only; uploads and storage
/// references will be added later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceRef {
    Url { url: String },
}

/// HTTP-level metadata learned about a source before or during download.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMetadata {
    /// MIME type from the Content-Type response header.
    pub content_type: Option<String>,
    /// MIME type from libmagic / infer byte sniffing.
    pub magic_mime: Option<String>,
    /// Byte length from Content-Length (may be absent or wrong for chunked).
    pub content_length: Option<u64>,
    /// Encoded source validator token.
    ///
    /// Encoding:
    /// - `E<value>` => upstream returned an `ETag: <value>` header
    /// - `M<value>` => upstream returned a `Last-Modified: <value>` header
    ///
    /// `E...` always takes precedence when both headers are present.
    pub etag: Option<String>,
    /// Last-Modified from the response, if present.
    pub last_modified: Option<String>,
    /// Whether the server indicated Accept-Ranges: bytes support.
    pub accepts_ranges: bool,
    /// Canonical file kind derived from magic bytes or Content-Type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_kind: Option<FileKind>,
    /// Canonical URL for this resource: final URL after redirects, with scheme
    /// and host normalised to lowercase and all query parameters stripped.
    ///
    /// This is the stable identity of the resource — independent of expiring
    /// presigned tokens, CDN cache-busting parameters, or other noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    /// Stable lookup token used for response caching.
    ///
    /// Currently equal to `canonical_url`.  Future versions will incorporate
    /// a scoping component (e.g. hashed account ID) so that entries from
    /// different tenants cannot collide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
}
