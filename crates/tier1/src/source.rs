//! Source reference and URL utilities.
//!
//! A `SourceRef` is the caller's pointer to a piece of media.  Currently that
//! is always a URL; uploads and object-store references will be added later.
//!
//! HTTP-level metadata (content type, content length, accept-ranges, …) is
//! read directly from `HttpBuffer` during the connect step.  The fields that
//! need to persist after the connection closes (`final_url`, the upstream etag)
//! are stored in `ThumbTrace`.

use std::time::SystemTime;

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
/// Extract a server-provided content hash from HTTP response headers.
///
/// Many storage services advertise a stable content hash that is a better
/// cache key than the URL (which may contain signing tokens or be content-
/// addressed but with different paths).  The returned `(value, source)` pair
/// gives the raw hash string and a short label naming which header it came
/// from.  Combined with a customer id before hashing, this produces a stable
/// storage key that is independent of URL shape.
///
/// Priority (highest to lowest):
/// 1. `x-amz-checksum-sha256` — AWS S3 / CloudFront SHA-256 (already a hash)
/// 2. `content-md5`           — RFC 1864 base64-encoded MD5
/// 3. Strong `etag`           — no `W/` prefix; S3/GCS use MD5 hex or SHA-256
/// 4. `x-goog-hash`           — GCS `md5=<base64>` or `crc32c=<base64>` directive
///
/// Returns `None` when none of these headers are present or usable.
pub fn content_hash_from_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Option<(String, &'static str)> {
    // 1. AWS SHA-256 checksum
    if let Some(v) = headers.get("x-amz-checksum-sha256") {
        let v = v.trim();
        if !v.is_empty() { return Some((v.to_string(), "x-amz-checksum-sha256")); }
    }

    // 2. RFC Content-MD5
    if let Some(v) = headers.get("content-md5") {
        let v = v.trim();
        if !v.is_empty() { return Some((v.to_string(), "content-md5")); }
    }

    // 3. Strong ETag (S3, GCS, and most CDNs emit MD5 or SHA-256 here)
    if let Some(v) = headers.get("etag") {
        let v = v.trim().trim_matches('"');
        if !v.is_empty() && !v.starts_with("W/") {
            return Some((v.to_string(), "etag"));
        }
    }

    // 4. Google Cloud Storage x-goog-hash (may have multiple directives)
    if let Some(v) = headers.get("x-goog-hash") {
        // Value is comma-separated: "crc32c=n03x6A==, md5=rL0Y20zC+Fzt72VPzMSk2A=="
        for part in v.split(',') {
            let part = part.trim();
            if let Some(hash) = part.strip_prefix("md5=") {
                let hash = hash.trim();
                if !hash.is_empty() { return Some((hash.to_string(), "x-goog-hash/md5")); }
            }
        }
        // Fall through to crc32c only if no md5
        for part in v.split(',') {
            let part = part.trim();
            if let Some(hash) = part.strip_prefix("crc32c=") {
                let hash = hash.trim();
                if !hash.is_empty() { return Some((hash.to_string(), "x-goog-hash/crc32c")); }
            }
        }
    }

    None
}
/// Extract a freshness token from HTTP response headers.
///
/// Prefers `ETag` over `Last-Modified`.  The returned string is opaque — its
/// leading character encodes the kind so [`conditional_headers`] can reconstruct
/// the right request header without the caller needing to track that separately:
/// - `E…` → was an ETag
/// - `M…` → was a Last-Modified value
///
/// Returns `None` if neither header is present.
#[deprecated(note = "use CacheHints::from_response_headers instead")]
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
#[deprecated(note = "use CacheHints::to_conditional instead")]
pub fn conditional_headers(etag: &str) -> Option<(&'static str, &str)> {
    match etag.as_bytes().first() {
        Some(b'E') => Some(("if-none-match", &etag[1..])),
        Some(b'M') => Some(("if-modified-since", &etag[1..])),
        _ => None,
    }
}

// ── CacheHints ────────────────────────────────────────────────────────────────

/// Structured cache freshness hints derived from upstream HTTP response headers.
///
/// Returned to callers as part of [`crate::result::ThumbResult`] and accepted
/// back on subsequent [`crate::request::ThumbObject`] requests, enabling both
/// client-side and server-side freshness fast paths without encoding tricks.
///
/// # Client-side fast path
///
/// Before re-requesting a thumbnail, check `is_fresh()`.  If the hints say the
/// resource is still fresh, the client can skip the request entirely.
///
/// # Server-side fast path
///
/// When a client sends `hints` back and `is_fresh()` is true, the pipeline
/// returns `NotModified` immediately — no upstream HTTP call, no cache lookup.
///
/// # Conditional requests
///
/// When the resource is stale, `to_conditional()` produces the `If-None-Match`
/// or `If-Modified-Since` header for an upstream revalidation request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheHints {
    /// Unix timestamp (seconds) after which the resource should be considered
    /// stale.  Derived from `Cache-Control: max-age` (or `s-maxage`) minus the
    /// `Age` header at fetch time.  `None` means no explicit freshness window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,

    /// `Cache-Control: stale-while-revalidate` window in seconds.
    /// When set, the client may serve a stale response for this many extra
    /// seconds while triggering a background refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_while_revalidate: Option<u32>,

    /// True when `Cache-Control: immutable` was present.
    /// The resource will not change within its freshness window; clients should
    /// skip revalidation until `expires_at` elapses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub immutable: bool,

    /// Raw upstream `ETag` value (with surrounding quotes).
    /// Used to construct `If-None-Match` on revalidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,

    /// Raw upstream `Last-Modified` value.
    /// Used to construct `If-Modified-Since` when no ETag is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl CacheHints {
    /// Parse freshness hints from HTTP response headers.
    ///
    /// Returns `None` when none of the recognised headers are present or
    /// contain useful information (e.g. `Cache-Control: no-store` only).
    pub fn from_response_headers(headers: &std::collections::HashMap<String, String>) -> Option<Self> {
        let mut hints = Self::default();
        let mut any = false;

        // ── Cache-Control ─────────────────────────────────────────────────────
        if let Some(cc) = headers.get("cache-control") {
            let mut max_age: Option<u64> = None;
            for directive in cc.split(',').map(str::trim) {
                let (key, val) = if let Some((k, v)) = directive.split_once('=') {
                    (k.trim(), Some(v.trim()))
                } else {
                    (directive, None)
                };
                match key.to_ascii_lowercase().as_str() {
                    "s-maxage" => {
                        if let Some(v) = val.and_then(|v| v.parse::<u64>().ok()) {
                            max_age = Some(v); // s-maxage takes priority
                        }
                    }
                    "max-age" if max_age.is_none() => {
                        if let Some(v) = val.and_then(|v| v.parse::<u64>().ok()) {
                            max_age = Some(v);
                        }
                    }
                    "immutable" => {
                        hints.immutable = true;
                        any = true;
                    }
                    "stale-while-revalidate" => {
                        if let Some(v) = val.and_then(|v| v.parse::<u32>().ok()) {
                            hints.stale_while_revalidate = Some(v);
                            any = true;
                        }
                    }
                    _ => {}
                }
            }

            if let Some(age_secs) = max_age {
                let age_consumed = headers
                    .get("age")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(0);
                let remaining = age_secs.saturating_sub(age_consumed);
                let now = unix_now_secs();
                hints.expires_at = Some(now + remaining);
                any = true;
            }
        }

        // ── ETag ──────────────────────────────────────────────────────────────
        if let Some(v) = headers.get("etag") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                hints.etag = Some(v);
                any = true;
            }
        }

        // ── Last-Modified ─────────────────────────────────────────────────────
        if let Some(v) = headers.get("last-modified") {
            let v = v.trim().to_string();
            if !v.is_empty() {
                hints.last_modified = Some(v);
                any = true;
            }
        }

        if any { Some(hints) } else { None }
    }

    /// Produce the conditional-request `(header-name, value)` for revalidation.
    ///
    /// Prefers `If-None-Match` (ETag) over `If-Modified-Since`.
    /// Returns `None` when neither validator is present.
    pub fn to_conditional(&self) -> Option<(&'static str, &str)> {
        if let Some(ref etag) = self.etag {
            return Some(("if-none-match", etag.as_str()));
        }
        if let Some(ref lm) = self.last_modified {
            return Some(("if-modified-since", lm.as_str()));
        }
        None
    }

    /// Returns `true` if the resource should still be considered fresh.
    ///
    /// Freshness is determined solely from `expires_at`.  When `expires_at` is
    /// `None` the resource has no explicit freshness window and is always stale.
    pub fn is_fresh(&self) -> bool {
        self.expires_at.map_or(false, |exp| unix_now_secs() < exp)
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
