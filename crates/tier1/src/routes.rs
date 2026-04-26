//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`                — liveness probe
//! - `GET  /thumb.jpeg?url=<url>`  — single thumbnail; returns raw JPEG bytes (canonical)
//! - `GET  /thumb?url=<url>`       — same handler; alias without extension
//! - `POST /batch`                 — batch thumbnail + describe; waits for all items, returns one JSON object
//! - `POST /stream`                — same input, but results stream as they complete (SSE or NDJSON)

use axum::{
    Json,
    extract::Query,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use std::convert::Infallible;

use crate::cook::{ThumbCook, ThumbSpec};
use crate::http_buf::PlatformStream;
use crate::request::{CallRequest, ThumbInput};
use crate::result::{JobStatus, ThumbResult};

// ── GET /health ───────────────────────────────────────────────────────────────

/// Liveness probe.  Always returns `{"status":"ok"}`.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

// ── GET /thumb ────────────────────────────────────────────────────────────────

/// Single-URL thumbnail endpoint.
///
/// # Request
///
/// ```text
/// GET /thumb.jpeg?url=http%3A%2F%2Fexample.com%2Fimage.jpg
/// If-None-Match: <etag>   # optional — supply the ETag returned by a prior response
/// ```
///
/// The `.jpeg` suffix on the path is the canonical form — it allows CDNs, social
/// media unfurlers, and image-aware middleware to identify the response as a JPEG
/// image from the URL alone without fetching it.  `/thumb` is an alias that maps
/// to the same handler for callers that prefer extension-free URLs.
///
/// **CDN note**: if routing through a CDN, ensure the cache key includes the full
/// query string.  Without this, all `?url=…` values would collapse to one cached
/// response.
///
/// # Response
///
/// | Status | Body                | Meaning                            |
/// |--------|---------------------|------------------------------------|
/// | 200    | JPEG bytes          | Thumbnail produced                 |
/// | 304    | empty               | Source unchanged (etag matched)    |
/// | 400    | JSON error          | Bad request (missing/bad URL)      |
/// | 404    | JSON error          | Source not found                   |
/// | 500    | JSON error          | Pipeline or upstream server error  |
#[derive(serde::Deserialize)]
pub struct ThumbQuery {
    pub url: String,
}

pub async fn thumb(
    Query(q): Query<ThumbQuery>,
    headers: HeaderMap,
) -> axum::response::Response {
    if q.url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "url parameter is required" })))
            .into_response();
    }
    if q.url.starts_with("file://") {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "file:// URLs are not permitted" })))
            .into_response();
    }

    // Only If-None-Match is accepted.  Our ETag tokens are opaque and already
    // encode the header kind (E… = ETag, M… = Last-Modified); pass through as-is.
    let etag: Option<String> = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let spec = ThumbSpec { url: q.url, etag, allow_local: false };
    let (result, _trace) = ThumbCook::<PlatformStream>::new(spec).run().await;

    match result.status {
        JobStatus::NotModified => StatusCode::NOT_MODIFIED.into_response(),

        JobStatus::Success if !result.thumbnail.is_empty() => {
            // Always emit ETag regardless of what the origin provided.  Our
            // opaque token (E… or M…) is the value clients must round-trip.
            let mut resp_headers = axum::http::HeaderMap::new();
            if let Some(ref tok) = result.etag {
                if let Ok(hv) = axum::http::HeaderValue::from_str(tok) {
                    resp_headers.insert(header::ETAG, hv);
                }
            }
            (
                StatusCode::OK,
                resp_headers,
                [(header::CONTENT_TYPE, "image/jpeg")],
                Bytes::from(result.thumbnail),
            )
                .into_response()
        }

        JobStatus::Failed => {
            let status = if result.message.contains("not found") {
                StatusCode::NOT_FOUND
            } else if result.message.contains("permission denied") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": result.message }))).into_response()
        }

        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": result.message })),
        )
            .into_response(),
    }
}

// ── POST /batch ───────────────────────────────────────────────────────────────

/// Batch thumbnail / describe endpoint.
///
/// Accepts a `CallRequest` JSON body and returns a JSON array of `ThumbResponse`
/// values.  Results arrive in completion order; streaming (NDJSON/SSE) will
/// deliver the same shape, just with earlier items arriving sooner.
pub async fn batch(
    Json(req): Json<CallRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut pool = FuturesUnordered::new();
    for input in req.items {
        let (url, etag) = input.into_parts();
        // Reject file:// at the HTTP boundary — never expose the server
        // filesystem to remote callers.  allow_local is intentionally absent
        // here; the pipeline::connect step enforces this independently too.
        if url.starts_with("file://") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "file:// URLs are not permitted" })),
            ));
        }
        let spec = ThumbSpec { url, etag, allow_local: false };
        pool.push(ThumbCook::<PlatformStream>::new(spec).run());
    }

    let mut items = Vec::with_capacity(pool.len());
    while let Some((result, _trace)) = pool.next().await {
        // trace is intentionally discarded here; the HTTP response only
        // returns the public ThumbResult.  Emit to a log sink when telemetry
        // is wired up.
        items.push(result);
    }

    Ok(Json(json!({ "items": items })))
}

// ── POST /stream ──────────────────────────────────────────────────────────────

/// Streaming thumbnail endpoint.
///
/// Accepts the same `CallRequest` body as `/batch`.  Results are emitted as
/// they complete rather than waiting for all items.
///
/// Content negotiation via the `Accept` header:
/// - `text/event-stream` → Server-Sent Events (SSE).  Each event has type
///   `item` and carries a JSON `ThumbResult` in the `data` field.
/// - `application/x-ndjson` (default) → newline-delimited JSON, one
///   `ThumbResult` object per line.
///
/// When a URL resolves to a T2/T3 format, **two** events are emitted for that
/// item: first a `status: rendering` intermediate event containing a
/// placeholder thumbnail (so the client has something to display immediately),
/// then a final event once the higher-tier renderer completes.  Clients that
/// do not handle streaming, or that only want completed results, should use
/// `POST /batch` instead or filter events where `status == "rendering"`.
pub async fn stream(
    headers: HeaderMap,
    Json(req): Json<CallRequest>,
) -> axum::response::Response {
    use axum::http;
    use axum::response::{IntoResponse, Sse};
    use axum::response::sse::Event;
    use tokio::sync::mpsc;

    let use_sse = headers
        .get(http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false);

    let (tx, rx) = mpsc::channel::<ThumbResult>(64);

    for input in req.items {
        let tx = tx.clone();
        tokio::spawn(async move { stream_one(input, tx).await });
    }
    // Drop the original sender so the channel closes once all spawned tasks
    // finish.  Each task holds its own clone.
    drop(tx);

    // Wrap the mpsc receiver as a Stream.
    let result_stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    if use_sse {
        let sse_stream = result_stream.map(|item| {
            let data = serde_json::to_string(&item).unwrap_or_default();
            Ok::<Event, Infallible>(Event::default().event("item").data(data))
        });
        Sse::new(sse_stream).into_response()
    } else {
        // NDJSON — one JSON object per line.
        let body_stream = result_stream.map(|item| {
            let mut line = serde_json::to_string(&item).unwrap_or_default();
            line.push('\n');
            Ok::<Bytes, Infallible>(Bytes::from(line))
        });
        axum::response::Response::builder()
            .header(http::header::CONTENT_TYPE, "application/x-ndjson")
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap()
    }
}

/// Drive the pipeline for one item and send intermediate + final events to
/// the channel.
///
/// Tier-1 eligible items emit a single final event.  T2/T3-bound items emit
/// a `Rendering` placeholder first, then a final (or deferred) event.
async fn stream_one(input: ThumbInput, tx: tokio::sync::mpsc::Sender<ThumbResult>) {
    let (url, etag) = input.into_parts();

    // Enforce file:// restriction at the HTTP boundary.
    if url.starts_with("file://") {
        let r = ThumbResult {
            url,
            message: "file:// URLs are not permitted".to_string(),
            ..ThumbResult::default()
        };
        let _ = tx.send(r).await;
        return;
    }

    let spec = ThumbSpec { url, etag, allow_local: false };
    let mut cook = ThumbCook::<PlatformStream>::new(spec);

    crate::pipeline::connect(&mut cook).await;
    if cook.http.is_none() {
        let _ = tx.send(cook.response).await;
        return;
    }

    crate::pipeline::inspect(&mut cook).await;

    if cook.trace.job_tier > 1 {
        // Emit the placeholder immediately so the client has something to show
        // while the higher-tier renderer works.  The `placeholder` field on the
        // result carries the icon token set by the inspect step.
        let mut placeholder = cook.response.clone();
        placeholder.status = JobStatus::Rendering;
        let _ = tx.send(placeholder).await;

        // TODO: forward `cook` to T2/T3 and await the real result.
        // For now, report that this item needs a higher-tier renderer.
        cook.response.status = JobStatus::DeferServer;
        cook.response.message =
            "deferred to higher-tier renderer (not yet connected)".to_string();
    }

    let _ = tx.send(cook.response).await;
}
