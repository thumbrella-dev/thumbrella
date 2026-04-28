//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`                — liveness probe
//! - `GET  /thumb.jpeg?url=<url>`  — single thumbnail; returns raw JPEG bytes (canonical)
//! - `GET  /thumb?url=<url>`       — same handler; alias without extension
//! - `POST /batch`                 — batch thumbnail + describe; waits for all items, returns one JSON object

use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value};

use crate::cook::{InputSpec, Runtime, ThumbCook};
use crate::http_buf::PlatformStream;
use crate::request::CallRequest;
use crate::result::JobStatus;

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
    State(runtime): State<Arc<Runtime>>,
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

    let etag: Option<String> = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let input = InputSpec { url: q.url, etag, allow_local: false };
    let (result, _trace, mut after) = ThumbCook::<PlatformStream>::from_input(input, runtime).run().await;
    after.drain_spawn();

    if result.status == JobStatus::NotModified {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    if result.thumbnail.is_empty() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": result.message.unwrap_or_default() })))
            .into_response();
    }

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

// ── POST /batch ───────────────────────────────────────────────────────────────

/// Batch thumbnail / describe endpoint.
///
/// Accepts a `CallRequest` JSON body and returns a JSON array of `ThumbResult`
/// values.  Results arrive in completion order; streaming (NDJSON/SSE) will
/// deliver the same shape, just with earlier items arriving sooner.
pub async fn batch(
    State(runtime): State<Arc<Runtime>>,
    Json(req): Json<CallRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut pool = FuturesUnordered::new();
    for input in req.items {
        let (url, etag) = input.into_parts();
        if url.starts_with("file://") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "file:// URLs are not permitted" })),
            ));
        }
        let spec = InputSpec { url, etag, allow_local: false };
        pool.push(ThumbCook::<PlatformStream>::from_input(spec, Arc::clone(&runtime)).run());
    }

    let mut items = Vec::with_capacity(pool.len());
    while let Some((result, _trace, mut after)) = pool.next().await {
        after.drain_spawn();
        items.push(result);
    }

    Ok(Json(json!({ "items": items })))
}
