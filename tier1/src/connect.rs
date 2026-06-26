//! Connect-string parsing — shared between native and WASM builds.
//!
//! Follows the same grammar as `parseConnect()` in the TypeScript client
//! and is used by both `TBR_CONNECT`, `TBR_TIER2`, and `TBR_TIER3`.
//!
//! ## Syntax
//!
//! ```text
//! <url>,<header>...
//! ```
//!
//! - **URL only:** `http://tier2:8000`
//! - **URL + key=value headers:** `http://tier2:8000,x-custom=hdr`
//! - **URL + auth token:** `http://tier2:8000,tbr_s_AbCd...`
//!   Tokens starting with `tbr_[a-z]_` are recognised as Bearer auth.
//! - **URL + handshake:** `http://tier2:8000,mysecret`
//!   Any bare value that does *not* look like an auth token is treated as
//!   an `x-tbr-handshake` header — a shorthand for the common case.
//! - **Backward-compat `#` fragment:** `http://tier2:8000#secret`
//!   (converted to `x-tbr-handshake: secret`)
//! - **Bare token:** `tbr_s_AbCd...` → `Authorization: Bearer`
//!   `mysecret` → `x-tbr-handshake: mysecret` (no URL)

use std::collections::HashMap;

/// Parsed connect-string target — URL plus optional HTTP headers.
///
/// Follows the same syntax as `TBR_CONNECT` in the TypeScript client.
#[derive(Debug, Clone, Default)]
pub struct ConnectTarget {
    /// Base URL of the handoff server (without trailing slash).
    pub url: Option<String>,
    /// Additional HTTP headers to send with every handoff request.
    pub headers: HashMap<String, String>,
}

/// Parse a connect string into a [`ConnectTarget`].
///
/// Follows the same grammar as `parseConnect()` in the TypeScript client:
///
/// - Plain URL: `http://tier2:8000`
/// - URL with `key=value` headers: `http://tier2:8000,x-tbr-handshake=s3cret`
/// - URL with auth token (recognised by `tbr_[a-z]_` prefix):
///   `http://tier2:8000,tbr_s_AbCd...` → `Authorization: Bearer`
/// - URL with bare handshake value: `http://tier2:8000,mysecret`
///   → `x-tbr-handshake: mysecret`
/// - Backward-compat `#` fragment: `http://tier2:8000#secret`
///   (fragment is converted to `x-tbr-handshake: secret` header)
///
/// Bare tokens without a `://` URL:
/// - Auth token (`tbr_[a-z]_` prefix) → `Authorization: Bearer <token>`, `url: None`
/// - Otherwise → `x-tbr-handshake: <value>`, `url: None`
pub fn parse_connect_target(raw: Option<String>) -> ConnectTarget {
    let Some(raw) = raw else { return ConnectTarget::default(); };

    // Bare value (no scheme) — either an auth token or a handshake.
    if !raw.contains("://") {
        let mut headers = HashMap::new();
        if looks_like_auth_token(&raw) {
            headers.insert("Authorization".to_string(), format!("Bearer {raw}"));
        } else {
            headers.insert("x-tbr-handshake".to_string(), raw.to_string());
        }
        return ConnectTarget { url: None, headers };
    }

    // Split on first comma to separate URL from optional header suffix.
    let (url_part, suffix) = match raw.split_once(',') {
        Some((u, s)) => (u, s),
        None => (raw.as_str(), ""),
    };

    let mut headers = HashMap::new();

    // Backward compatibility: if the URL part contains a `#` fragment,
    // treat it as an `x-tbr-handshake` header value (old syntax).
    let base_url = if let Some((base, fragment)) = url_part.split_once('#') {
        let frag = fragment.trim();
        if !frag.is_empty() {
            headers.insert("x-tbr-handshake".to_string(), frag.to_string());
        }
        base.trim()
    } else {
        url_part.trim()
    };

    // Parse comma-separated header segments.
    for seg in suffix.split(',') {
        let s = seg.trim();
        if s.is_empty() {
            continue;
        }
        if let Some((k, v)) = s.split_once('=') {
            headers.insert(k.trim().to_string(), v.trim().to_string());
        } else if looks_like_auth_token(s) {
            headers.insert("Authorization".to_string(), format!("Bearer {s}"));
        } else {
            headers.insert("x-tbr-handshake".to_string(), s.to_string());
        }
    }

    let url = if base_url.is_empty() {
        None
    } else {
        Some(base_url.trim_end_matches('/').to_string())
    };

    ConnectTarget { url, headers }
}

/// Check if a value looks like a Thumbrella auth token.
///
/// Auth tokens follow the pattern `tbr_[a-z]_` + base64url body (e.g.
/// `tbr_s_...` for secret, `tbr_p_...` for publishable).  If a handshake
/// value matches this prefix, it was almost certainly set by mistake.
pub(crate) fn looks_like_auth_token(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() >= 6
        && b.starts_with(b"tbr_")
        && b[4].is_ascii_lowercase()
        && b[5] == b'_'
}
