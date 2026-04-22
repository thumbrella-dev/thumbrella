//! HTTP route handlers.

use axum::{
    Json,
    extract::ConnectInfo,
    http::StatusCode,
    response::Html,
};
use axum_extra::extract::Query;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use crate::{
    BatchRequest,
    BatchResponse,
    RequestRecord,
    app_config,
    devpage,
};

use crate::pipeline;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_request_id() -> String {
    format!("batch-{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, (StatusCode, Json<Value>)> {
    let start = Instant::now();
    let mut request_record = RequestRecord {
        id: new_request_id(),
        customer: "unknown".to_string(),
        host: addr.ip().to_string(),
        path: "/batch".to_string(),
        timestamp: now_rfc3339(),
        method: Some("POST".to_string()),
        user_agent: None,
        duration_secs: None,
    };

    let items = req.into_items();
    if items.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "items must not be empty" })),
        ));
    }

    let profile = app_config().thumbnail_profile();
    let developer_mode = app_config().developer_mode;
    let mut tasks = Vec::with_capacity(items.len());
    for item in &items {
        tasks.push(pipeline::process_item(item, &profile, &request_record.id));
    }

    let mut item_results = futures::future::join_all(tasks).await;
    request_record.duration_secs = Some(start.elapsed().as_secs_f64());

    // Strip server tracking from batch responses unless developer mode is on.
    if !developer_mode {
        for result in &mut item_results {
            result.server = None;
        }
    }

    Ok(Json(BatchResponse::from_item_results(item_results, request_record)))
}

/// GET /dev
///
/// Developer helper endpoint that accepts repeated `url` query params and
/// renders a simple HTML page containing thumbnail previews and item stats.
pub async fn dev(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<DevQuery>,
) -> Html<String> {
    let start = Instant::now();
    let mut request_record = RequestRecord {
        id: format!("dev-{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)),
        customer: "unknown".to_string(),
        host: addr.ip().to_string(),
        path: "/dev".to_string(),
        timestamp: now_rfc3339(),
        method: Some("GET".to_string()),
        user_agent: None,
        duration_secs: None,
    };

    let urls: Vec<String> = query
        .url
        .into_iter()
        .filter(|v| !v.is_empty())
        .collect();

    let html = devpage::render(urls, &mut request_record, start).await;
    Html(html)}