//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`  — liveness probe
//! - `POST /batch`   — batch thumbnail + describe request

use axum::{
    Json,
    http::StatusCode,
};
use serde_json::{json, Value};

use crate::request::{BatchRequest, ItemRequest};
use crate::result::ItemResponse;
use crate::pipeline;

// ── GET /health ───────────────────────────────────────────────────────────────

/// Liveness probe.  Always returns `{"status":"ok"}`.
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

// ── POST /batch ───────────────────────────────────────────────────────────────

/// Batch thumbnail / describe endpoint.
///
/// Accepts a `BatchRequest` JSON body and returns a JSON array of `ItemResult`
/// values.  Results arrive in completion order; streaming (NDJSON/SSE) will
/// deliver the same shape, just with earlier items arriving sooner.
pub async fn batch(
    Json(req): Json<BatchRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut items: Vec<ItemResponse> = Vec::with_capacity(req.items.len());

    for input in req.items {
        let item = ItemRequest::from_input(input);
        items.push(pipeline::process_item(item).await);
    }

    Ok(Json(json!({ "items": items })))
}
