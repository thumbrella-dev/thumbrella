//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`                — liveness probe
//! - `GET  /thumb.jpeg?url=<url>`  — single thumbnail; returns raw JPEG bytes (canonical)
//! - `GET  /thumb?url=<url>`       — same handler; alias without extension
//! - `POST /handoff`               — trusted tier-to-tier thumbnail handoff
//! - `POST /batch`                 — batch thumbnail + describe; waits for all items, returns one JSON object
//!
//! # Server token
//!
//! When `TBR_HANDSHAKE` is set, all endpoints require the
//! `x-tbr-handshake` header.  Use [`require_handshake`] as an axum
//! middleware layer to enforce this uniformly.

use std::{convert::Infallible, sync::Arc};

use axum::{
    body::Body,
    Json,
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::stream::{self, FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use tokio::{sync::mpsc, task::JoinSet};

use crate::cook::{InputSpec, Runtime, ThumbCook};
use crate::cache::CacheStore;
use crate::http_buf::PlatformStream;
use crate::handoff::{HANDSHAKE_HEADER, HandoffResponse, ThumbHandoff};
use crate::request::CallRequest;
use crate::result::ResultSource;
use crate::source::CacheHints;
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
/// | ResultStatus | Body                | Meaning                            |
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
    let url = match normalize_url(q.url, runtime.allow_local) {
        Ok(u)    => u,
        Err(msg) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response(),
    };

    let cache: Option<CacheHints> = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|etag| CacheHints { etag: Some(etag.to_owned()), ..Default::default() });

    let input = InputSpec { url, cache, allow_local: runtime.allow_local };
    let (result, _trace, mut after) = ThumbCook::<PlatformStream>::from_input(input, runtime).run().await;
    after.drain_spawn();

    if result.source == Some(ResultSource::NotModified) {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let thumb = result.media.as_ref().map(|m| &m.thumbnail);
    if thumb.map_or(true, |t| t.is_empty()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": result.message.unwrap_or_default() })))
            .into_response();
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/jpeg")],
        Bytes::from(thumb.unwrap().clone()),
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
        let (url, cache) = input.into_parts();
        let url = match normalize_url(url, runtime.allow_local) {
            Ok(u)    => u,
            Err(msg) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response(),
        };
        jobs.push((idx, InputSpec { url, cache, allow_local: runtime.allow_local }));
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

/// Validate and normalise a URL from a request query parameter.
///
/// When `allow_local` is `false` (default):
/// - `file://` URLs → rejected with 400.
/// - Bare paths (no `://` scheme) → rejected with 400.
///
/// When `allow_local` is `true` (`TBR_ALLOW_FILES=1`):
/// - `file://` URLs → accepted unchanged.
/// - Bare absolute paths (starting with `/`) → promoted to `file://` URLs
///   (e.g. `/data/img.png` becomes `file:///data/img.png`).
/// - Bare relative paths → rejected; the server's CWD is ambiguous.
fn normalize_url(url: String, allow_local: bool) -> Result<String, &'static str> {
    if url.contains("://") {
        if url.starts_with("file://") && !allow_local {
            Err("file:// URLs are not permitted")
        } else {
            Ok(url)
        }
    } else if allow_local {
        if url.starts_with('/') {
            Ok(format!("file://{url}"))
        } else {
            Err("bare relative paths are not permitted; use an absolute path or a file:// URL")
        }
    } else {
        Err("url must be an http:// or https:// URL")
    }
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
///
/// Handshake auth is enforced by the [`require_handshake`] middleware
/// layer; this handler does not perform its own auth check.
pub async fn handoff(
    State(runtime): State<Arc<Runtime>>,
    Json(payload): Json<ThumbHandoff>,
) -> axum::response::Response {

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
        if let Some(ref mut media) = result.media {
            media.thumbnail.clear();
        }
    }

    let body = HandoffResponse { result, trace };
    (StatusCode::OK, Json(body)).into_response()
}

// ── Server-token middleware ───────────────────────────────────────────────────

/// Axum middleware that enforces `TBR_HANDSHAKE` on every endpoint.
///
/// When [`Runtime::handshake`] is `None` (the default), all requests pass
/// through unauthenticated.  When set, every request must include a matching
/// `x-tbr-handshake` header or receive a 401 response.
///
/// Apply as a layer in `run_server`:
/// ```ignore
/// let app = Router::new()
///     .route(…)
///     .layer(axum::middleware::from_fn_with_state(
///         runtime.clone(),
///         routes::require_handshake,
///     ))
///     .with_state(runtime);
/// ```
pub async fn require_handshake(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = runtime.handshake.as_deref() else {
        return next.run(request).await;
    };

    let provided = headers
        .get(HANDSHAKE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if provided != expected {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid handshake"})),
        ).into_response();
    }

    next.run(request).await
}
