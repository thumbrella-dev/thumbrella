//! HTTP route handlers.

use axum::{
    Json,
    http::StatusCode,
    response::Html,
};
use axum_extra::extract::Query;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{
    BatchRequest,
    BatchResponse,
    app_config,
    devpage,
};

use crate::pipeline;

#[derive(Debug, Default, Deserialize)]
pub struct DevQuery {
    #[serde(default)]
    url: Vec<String>,
}

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

    let profile = app_config().thumbnail_profile();
    let mut tasks = Vec::with_capacity(items.len());
    for item in &items {
        tasks.push(pipeline::process_item(item, &profile));
    }

    let item_results = futures::future::join_all(tasks).await;

    Ok(Json(BatchResponse::from_item_results(item_results)))
}

/// GET /dev
///
/// Developer helper endpoint that accepts repeated `url` query params and
/// renders a simple HTML page containing thumbnail previews and item stats.
pub async fn dev(Query(query): Query<DevQuery>) -> Html<String> {
    let urls: Vec<String> = query
        .url
        .into_iter()
        .filter(|v| !v.is_empty())
        .collect();

    devpage::render(urls).await
}