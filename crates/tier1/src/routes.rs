//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`                — liveness probe
//! - `GET  /thumb.jpeg?url=<url>`  — single thumbnail; returns raw JPEG bytes (canonical)
//! - `GET  /thumb?url=<url>`       — same handler; alias without extension
//! - `POST /handoff`               — trusted tier-to-tier thumbnail handoff
//! - `POST /batch`                 — batch thumbnail + describe; waits for all items, returns one JSON object

use std::{convert::Infallible, sync::Arc};

use axum::{
    body::Body,
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::stream::{self, FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use tokio::{sync::mpsc, task::JoinSet};

use crate::cook::{InputSpec, Runtime, ThumbCook};
use crate::cache::CacheStore;
use crate::http_buf::PlatformStream;
use crate::handoff::{HANDOFF_CODE_HEADER, HandoffResponse, ThumbHandoff};
use crate::request::CallRequest;
use crate::result::JobStatus;
use crate::tracelog::TraceStore;

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
    headers: HeaderMap,
    Json(req): Json<CallRequest>,
) -> Response {
    let stream_mode = wants_ndjson(&headers);

    let mut jobs = Vec::with_capacity(req.items.len());
    for (idx, input) in req.items.into_iter().enumerate() {
        let (url, etag) = input.into_parts();
        if url.starts_with("file://") {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "file:// URLs are not permitted" })),
            )
                .into_response();
        }
        jobs.push((idx, InputSpec { url, etag, allow_local: false }));
    }
    let count = jobs.len();

    if stream_mode {
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        let batch_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let _ = tx.send(as_ndjson_line(json!({
                "type": "batch.started",
                "count": count,
            })));

            let mut pending = JoinSet::new();
            for (idx, spec) in jobs {
                let _ = tx.send(as_ndjson_line(json!({
                    "type": "item.accepted",
                    "index": idx,
                })));

                let item_runtime = Arc::clone(&batch_runtime);
                let item_tx = tx.clone();
                pending.spawn(async move {
                    let progress_tx = item_tx.clone();
                    let progress = Box::new(move |result| {
                        let _ = progress_tx.send(as_ndjson_line(json!({
                            "type": "item.intermediate",
                            "index": idx,
                            "result": result,
                        })));
                    });

                    let (result, _trace, mut after) = ThumbCook::<PlatformStream>::from_input(spec, item_runtime)
                        .run_with_progress(Some(progress))
                        .await;
                    after.drain_spawn();
                    let _ = item_tx.send(as_ndjson_line(json!({
                        "type": "item.result",
                        "index": idx,
                        "result": result,
                    })));
                });
            }

            while pending.join_next().await.is_some() {}
            let _ = tx.send(as_ndjson_line(json!({ "type": "batch.complete" })));
        });

        let stream = stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|line| (Ok::<Bytes, Infallible>(line), rx))
        });
        let body = Body::from_stream(stream);
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-ndjson"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response();
    }

    let mut pool = FuturesUnordered::new();
    for (idx, spec) in jobs {
        let job_runtime = Arc::clone(&runtime);
        pool.push(async move {
            let (result, trace, after) = ThumbCook::<PlatformStream>::from_input(spec, job_runtime).run().await;
            (idx, result, trace, after)
        });
    }

    let mut items = Vec::with_capacity(count);
    while let Some((_idx, result, _trace, mut after)) = pool.next().await {
        after.drain_spawn();
        items.push(result);
    }

    Json(json!({ "items": items })).into_response()
}

fn wants_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            accept.split(',').any(|part| {
                let mime = part.split(';').next().unwrap_or("").trim();
                mime.eq_ignore_ascii_case("application/x-ndjson")
                    || mime.eq_ignore_ascii_case("application/ndjson")
            })
        })
}

fn as_ndjson_line(value: Value) -> Bytes {
    let mut buf = serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"type\":\"error\"}".to_vec());
    buf.push(b'\n');
    Bytes::from(buf)
}

// ── POST /handoff ────────────────────────────────────────────────────────────

/// Trusted tier-to-tier handoff endpoint.
///
/// Accepts a serialized [`ThumbHandoff`] payload from another tier, rebuilds a
/// cook, reconnects to the source URL, and runs render+deliver with cache
/// lookup/store disabled on this receiving side.
pub async fn handoff(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    Json(payload): Json<ThumbHandoff>,
) -> axum::response::Response {
    let Some(expected_code) = runtime.handoff_accept.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "handoff is not configured on this server" })),
        ).into_response();
    };

    let provided_code = headers
        .get(HANDOFF_CODE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided_code != expected_code {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid handoff credentials" })),
        ).into_response();
    }

    // Handoff cooks do not own cache reads/writes or trace emission.
    // The entry tier is the authority for cache + trace in this request chain.
    let mut handoff_runtime = (*runtime).clone();
    handoff_runtime.cache = CacheStore::none();
    handoff_runtime.trace = TraceStore::none();
    let handoff_runtime = Arc::new(handoff_runtime);

    let (mut result, trace, mut after) = ThumbCook::<PlatformStream>::from_handoff(payload, handoff_runtime).run().await;
    after.drain_spawn();

    // Save bandwidth on tier-to-tier responses: send placeholder token only.
    if result.placeholder.is_some() {
        result.thumbnail.clear();
    }

    let body = HandoffResponse { result, trace };
    (StatusCode::OK, Json(body)).into_response()
}
