//! HTTP route handlers.

use axum::{Json, http::StatusCode};
use serde_json::{json, Value};
use thumbrella_types::{BatchRequest, BatchResponse, ThumbnailProfile};

use crate::pipeline;

/// GET /health
pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// POST /batch
///
/// Synchronous endpoint: waits for all items and returns a JSON array.
/// Streaming (NDJSON / SSE) will be added once the pipeline iterator is solid.
pub async fn batch(
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, (StatusCode, Json<Value>)> {
    let items = req.into_items();
    if items.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "items must not be empty" })),
        ));
    }

    let profile = ThumbnailProfile::default();
    let mut tasks = Vec::with_capacity(items.len());
    for item in &items {
        tasks.push(pipeline::process_item(item, &profile));
    }

    let item_results = futures::future::join_all(tasks).await;

    Ok(Json(BatchResponse { items: item_results }))
}