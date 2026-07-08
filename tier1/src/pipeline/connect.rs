//! Pipeline step: **connect** — open the HTTP connection and capture headers.

use std::sync::Arc;

use crate::cook::{CookStatus, ThumbCook};
use crate::http_buf::{ConnectOptions, HttpBuffer, HttpStream};
use crate::source::{CacheHints, canonical_url};

/// Extract `scheme://host[:port]` from a URL (no trailing slash).
fn origin_of(url: &str) -> &str {
    let after_scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
    match url[after_scheme..].find('/') {
        Some(i) => &url[..after_scheme + i],
        None => url,
    }
}

/// Open the HTTP connection and capture response metadata.
///
/// On a successful 2xx response `cook.http_buf` is populated (via
/// `http_install`) and `cook.status` remains `Processing`.
///
/// All other outcomes set `cook.status` to a terminal variant (typically
/// `Failed` or `NotModified`); the pipeline stops.
///
/// Populates:
/// - `cook.media.file_size`       — from `Content-Length`
/// - `cook.http_headers`          — full response headers
/// - `cook.http_status`           — HTTP status code
/// - `cook.http_accepts_ranges`   — from `Accept-Ranges` header
/// - `cook.src.cache_hints`        — upstream freshness hints (parsed from headers)
/// - `cook.src.final_url`         — URL after any redirects
/// - `cook.src.canonical_url`     — query-stripped stable cache key
pub async fn connect<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let mut options = ConnectOptions::default();

    options
        .headers
        .push(("User-Agent".to_string(), cook.runtime.user_agent.clone()));

    // Apply conditional request headers from caller's prior cache hints.
    if let Some(ref hints) = cook.input.cache {
        if let Some((name, value)) = hints.to_conditional() {
            options.headers.push((name.to_string(), value.to_string()));
        }
    }

    // Enforce the file:// restriction — second line of defence after routes.rs.
    if cook.input.url.starts_with("file://") && !cook.input.allow_local {
        cook.fail("file:// URLs are not permitted");
        return;
    }

    // Short-circuit on back-off'd origins (429 / 503 rate-limiting window).
    let origin = origin_of(&cook.input.url);
    if let Some(cached_status) = cook.runtime.origin_backoff.check(origin).await {
        cook.http_status = cached_status;
        cook.status = CookStatus::Overloaded;
        return;
    }

    // Short-circuit on recently-failed URLs (5 s debounce window).
    if let Some((cached_status, msg)) = cook.runtime.url_failures.check(&cook.input.url).await {
        cook.http_status = cached_status;
        cook.fail(msg.as_ref());
        return;
    }

    let buf = match HttpBuffer::<S>::open(cook.input.url.clone(), options).await {
        Ok(buf) => buf,
        Err(e) => {
            cook.fail(e.to_string());
            return;
        }
    };

    cook.http_status = buf.status;
    cook.http_headers = buf.headers.clone();
    cook.http_accepts_ranges = buf.accepts_ranges;
    cook.media.file_size = buf.content_length;

    // 429 / 503 — rate limiting: engage origin back-off, return Unavailable.
    // Parse `Retry-After` (integer seconds only); fall back to default TTL.
    if matches!(buf.status, 429 | 503) {
        let ttl = buf
            .headers
            .get("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(cook.runtime.backoff_default);
        cook.runtime
            .origin_backoff
            .record(origin_of(&cook.input.url).to_string(), buf.status, ttl)
            .await;
        cook.status = CookStatus::Overloaded;
        return;
    }

    // Classify remaining non-2xx responses; record them in the URL failure cache.
    let fail_msg: Option<Arc<str>> = match buf.status {
        304 => {
            cook.status = CookStatus::Fresh;
            return;
        }
        401 | 403 => Some(Arc::from("permission denied")),
        404 => Some(Arc::from("source not found")),
        410 => Some(Arc::from("source gone")),
        s if s >= 500 => Some(Arc::from(format!("server error (HTTP {s})").as_str())),
        s if !(200..300).contains(&s) => Some(Arc::from(format!("unexpected HTTP status {s}").as_str())),
        _ => None,
    };

    if let Some(msg) = fail_msg {
        cook.runtime
            .url_failures
            .record(cook.input.url.clone(), buf.status, msg.clone())
            .await;
        cook.fail(msg.as_ref());
        return;
    }

    cook.src.cache_hints = CacheHints::from_response_headers(&buf.headers);
    cook.src.final_url = Some(buf.url.clone());
    cook.src.canonical_url = canonical_url(&buf.url);

    // If the server returned a 2xx HTML page (common for CDN error pages on
    // missing files), treat it as "not found" rather than trying to thumbnail
    // what looks like valid content.
    if let Some(ct) = buf.headers.get("content-type") {
        if ct.starts_with("text/html") {
            cook.fail("source returned HTML (likely an error page, not media)");
            return;
        }
    }

    cook.http_install(buf);
}
