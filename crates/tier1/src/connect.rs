//! Connect-string parsing — shared between native and WASM builds.
//!
//! Follows the same grammar as `parseConnect()` in the TypeScript client
//! and is used by both `TBR_CONNECT`, `TBR_TIER2`, and `TBR_TIER3`.

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
/// - URL with bare token (becomes Bearer): `http://tier2:8000,tok`
/// - Backward-compat `#` fragment: `http://tier2:8000#secret`
///   (fragment is converted to `x-tbr-handshake: secret` header)
///
/// Bare tokens without a `://` URL return `url: None` and the token
/// as `Authorization: Bearer <token>`.
pub fn parse_connect_target(raw: Option<String>) -> ConnectTarget {
    let Some(raw) = raw else { return ConnectTarget::default(); };

    // Bearer token — no scheme.
    if !raw.contains("://") {
        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {raw}"));
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
        } else {
            headers.insert("Authorization".to_string(), format!("Bearer {s}"));
        }
    }

    let url = if base_url.is_empty() {
        None
    } else {
        Some(base_url.trim_end_matches('/').to_string())
    };

    ConnectTarget { url, headers }
}
