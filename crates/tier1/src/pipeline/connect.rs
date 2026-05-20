//! Pipeline step: **connect** — open the HTTP connection and capture headers.

use crate::cook::{CookStatus, ThumbCook};
use crate::http_buf::{ConnectOptions, HttpBuffer, HttpStream};
use crate::source::{CacheHints, canonical_url};

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

    // Apply conditional request headers from caller's prior cache hints.
    if let Some(ref hints) = cook.input.hints {
        if let Some((name, value)) = hints.to_conditional() {
            options.headers.push((name.to_string(), value.to_string()));
        }
    }

    // Enforce the file:// restriction — second line of defence after routes.rs.
    if cook.input.url.starts_with("file://") && !cook.input.allow_local {
        cook.fail("file:// URLs are not permitted");
        return;
    }

    let buf = match HttpBuffer::<S>::open(cook.input.url.clone(), options).await {
        Ok(buf) => buf,
        Err(e) => {
            cook.fail(e.to_string());
            return;
        }
    };

    cook.http_status         = buf.status;
    cook.http_headers        = buf.headers.clone();
    cook.http_accepts_ranges = buf.accepts_ranges;
    cook.media.file_size     = buf.content_length;

    match buf.status {
        304 => {
            cook.status = CookStatus::NotModified;
            return;
        }
        401 | 403 => { cook.fail("permission denied"); return; }
        404        => { cook.fail("source not found");  return; }
        410        => { cook.fail("source gone");        return; }
        s if s >= 500               => { cook.fail(format!("server error (HTTP {s})")); return; }
        s if !(200..300).contains(&s) => { cook.fail(format!("unexpected HTTP status {s}")); return; }
        _ => {} // 2xx — continue
    }

    cook.src.cache_hints   = CacheHints::from_response_headers(&buf.headers);
    cook.src.final_url     = Some(buf.url.clone());
    cook.src.canonical_url = canonical_url(&buf.url);

    cook.http_install(buf);
}
