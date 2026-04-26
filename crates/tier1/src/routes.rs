//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`  — liveness probe
//! - `POST /batch`   — batch thumbnail + describe request

use axum::{
    Json,
    http::StatusCode,
};
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value};

use crate::cook::{ThumbCook, ThumbSpec};
use crate::http_buf::PlatformStream;
use crate::request::CallRequest;

// ── GET /health ───────────────────────────────────────────────────────────────

/// Liveness probe.  Always returns `{"status":"ok"}`.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
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
        let spec = ThumbSpec { url, etag };
        pool.push(ThumbCook::<PlatformStream>::new(spec).cook());
    }

    let mut items = Vec::with_capacity(pool.len());
    while let Some(result) = pool.next().await {
        items.push(result);
    }

    Ok(Json(json!({ "items": items })))
}
