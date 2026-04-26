//! Pipeline step: **connect** — open the HTTP connection and capture headers.

use crate::cook::ThumbCook;
use crate::http_buf::{ConnectOptions, HttpBuffer, HttpStream};
use crate::result::JobStatus;
use crate::source::{canonical_url, conditional_headers, etag_from_headers};

/// Open the HTTP connection and capture response metadata.
///
/// On a successful 2xx response `cook.http` is populated and the function
/// returns normally — subsequent pipeline steps continue.
///
/// All other outcomes set `cook.response` appropriately and leave
/// `cook.http = None`; the caller should treat a `None` http buffer as a
/// signal to stop the pipeline and return the current result.
///
/// Populates:
/// - `cook.response.file_size`   — from `Content-Length`
/// - `cook.response.etag`        — upstream freshness token (opaque)
/// - `cook.trace.source_etag`    — same value
/// - `cook.trace.final_url`      — URL after any redirects
/// - `cook.trace.canonical_url`  — query-stripped stable cache key
/// - `cook.trace.job_tier`       — 1
pub async fn connect<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let mut options = ConnectOptions::default();

    // Apply conditional request headers if the caller supplied a prior etag.
    if let Some(etag) = &cook.spec.etag {
        if let Some((name, value)) = conditional_headers(etag) {
            options.headers.push((name.to_string(), value.to_string()));
        }
    }

    // Enforce the file:// restriction as a second line of defence.
    // HTTP entry points must not set allow_local; this catches any that slip
    // through without the routes.rs check.
    if cook.spec.url.starts_with("file://") && !cook.spec.allow_local {
        cook.fail("file:// URLs are not permitted");
        return;
    }

    let buf = match HttpBuffer::<S>::open(cook.spec.url.clone(), options).await {
        Ok(buf) => buf,
        Err(e) => {
            cook.fail(e.to_string());
            return;
        }
    };

    // Map HTTP status to job outcome.
    // HttpStream::connect only returns Err on network-level failures; HTTP
    // error statuses arrive here so the pipeline can handle them with the
    // right user-facing messages.
    match buf.status {
        304 => {
            cook.response.status = JobStatus::NotModified;
            return;
        }
        401 | 403 => {
            cook.fail("permission denied");
            return;
        }
        404 => {
            cook.fail("source not found");
            return;
        }
        410 => {
            cook.fail("source gone");
            return;
        }
        s if s >= 500 => {
            cook.fail(format!("server error (HTTP {s})"));
            return;
        }
        s if !(200..300).contains(&s) => {
            cook.fail(format!("unexpected HTTP status {s}"));
            return;
        }
        _ => {} // 2xx — continue
    }

    // Populate result and trace from the response metadata.
    cook.response.file_size = buf.content_length;
    cook.trace.source_etag = etag_from_headers(&buf.headers);
    cook.response.etag = cook.trace.source_etag.clone();
    cook.trace.final_url = Some(buf.url.clone());
    cook.trace.canonical_url = canonical_url(&buf.url);
    cook.trace.job_tier = 1;

    cook.http = Some(buf);
}
