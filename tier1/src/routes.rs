//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`                       — liveness probe
//! - `GET  /placeholder/:kind.jpeg`       — static placeholder thumbnail for a file kind
//! - `GET  /thumb.jpeg?url=<url>`         — single thumbnail; returns raw JPEG bytes (canonical)
//! - `GET  /thumb?url=<url>`              — same handler; alias without extension
//! - `POST /handoff`                      — trusted tier-to-tier thumbnail handoff
//! - `POST /batch`                        — batch thumbnail + describe; waits for all items, returns one JSON object
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
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::stream::{self, FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{sync::mpsc, task::JoinSet};
use web_time::Instant;

use crate::cook::{InputSpec, Runtime, ThumbCook};
use crate::cache::CacheStore;
use crate::http_buf::PlatformStream;
use crate::handoff::{HANDSHAKE_HEADER, HandoffResponse, ThumbHandoff};
use crate::media::FileKind;
use crate::request::CallRequest;
use crate::result::ResultSource;
use crate::tracelog::TraceStore;
use crate::ux;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract a client IP from request headers or connection info.
fn client_ip(headers: &HeaderMap, connect_info: Option<&SocketAddr>) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| connect_info.map(|a| a.ip().to_string()))
}

/// Return a human-readable string for a FileKind.
fn kind_str(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Image    => "image",
        FileKind::Video    => "video",
        FileKind::Audio    => "audio",
        FileKind::Vector   => "vector",
        FileKind::Document => "document",
        FileKind::Geometry => "geometry",
        FileKind::Archive  => "archive",
        FileKind::Text     => "text",
        FileKind::Binary   => "binary",
        FileKind::Unknown  => "unknown",
    }
}

/// Return a human-readable label for a ResultSource.
fn source_label(source: &ResultSource) -> &'static str {
    match source {
        ResultSource::Render      => "render",
        ResultSource::Cache       => "cache",
        ResultSource::Shortcut    => "shortcut",
        ResultSource::Placeholder => "placeholder",
        ResultSource::Fallback    => "fallback",
        ResultSource::NotModified => "not_modified",
        ResultSource::Client      => "client_error",
    }
}

/// Log a completed thumbnail result through the UX layer.
fn log_result(result: &crate::ThumbResult, duration_ms: u64) {
    let ux = ux::get();
    let media = result.media.as_ref();
    // Use the remote HTTP status when available (reflects what the source
    // server returned), falling back to our result-status mapping.
    let status_code = result.http_status.unwrap_or_else(|| match result.status {
        crate::result::ResultStatus::Success     => 200,
        crate::result::ResultStatus::Failed      => 500,
        crate::result::ResultStatus::Overloaded  => 503,
        crate::result::ResultStatus::Intermediate => 102,
    });
    ux.log_thumb_result(
        &result.url,
        status_code,
        duration_ms,
        media.map(|m| kind_str(m.kind)),
        media.map(|m| m.extension.as_str()),
        result.source.as_ref().map(source_label),
        result.message.as_deref(),
    );
}

// ── GET /health ───────────────────────────────────────────────────────────────

/// Liveness probe.  Returns server metadata:
///
/// ```json
/// {"status":"ok","thumbrella":0}
/// ```
///
/// The `thumbrella` field is the major version only — no minor or patch,
/// for safety against version-based targeting.
///
/// Logging is rate-limited: after 20 health checks, further requests are
/// suppressed and a one-time hint is printed.
pub async fn health(
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
) -> Json<Value> {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    static HEALTH_COUNT: AtomicU32 = AtomicU32::new(0);
    static HINT_SHOWN: AtomicBool = AtomicBool::new(false);

    let full_log = matches!(std::env::var("TBR_LOG").as_deref(), Ok("full"));
    let n = HEALTH_COUNT.fetch_add(1, Ordering::Relaxed);
    let ip = client_ip(&headers, Some(&connect_info.0)).unwrap_or_else(|| "?".to_string());

    if full_log || n < 20 {
        let line = format!("GET /health from {ip}\n");
        let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
    } else if !HINT_SHOWN.swap(true, Ordering::Relaxed) {
        let line = "  # hint: no longer showing /health requests, use TBR_LOG=full to see them\n";
        let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
    }

    let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0);
    Json(json!({ "status": "ok", "thumbrella": major }))
}

// ── Landing page ─────────────────────────────────────────────────────────────

/// Returns a landing page at `/` with embedded logo and favicon.
///
/// When `TBR_HANDSHAKE` is set the thumbnail demo links are hidden (they
/// would fail without the handshake header) and the connect string includes
/// a placeholder for the secret.
pub async fn landing(State(runtime): State<Arc<Runtime>>) -> Response {
    let template = include_str!("landing.html");

    let has_handshake = runtime.handshake.is_some();

    let body_class = if has_handshake { "has-handshake" } else { "" };
    let connect = if has_handshake {
        "TBR_CONNECT=http://localhost:3114,**handshake*****"
    } else {
        "TBR_CONNECT=http://localhost:3114"
    };
    let cli_prefix = if has_handshake {
        "TBR_CONNECT=http://localhost:3114,**handshake***** "
    } else {
        "TBR_CONNECT=http://localhost:3114 "
    };

    let body = template
        .replace("{{BODY_CLASS}}", body_class)
        .replace("{{CONNECT}}", connect)
        .replace("{{CLI_PREFIX}}", cli_prefix);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

// ── Fallback — 404 for unknown routes ─────────────────────────────────────────

/// Catch-all for unmatched routes.  Logs the request and returns 404.
pub async fn not_found(
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
) -> Response {
    let ip = client_ip(&headers, Some(&connect_info.0));
    let line = format!(
        "{} {} from {} - 404 not found\n",
        colour::cyan(method.as_str()),
        uri.path(),
        ip.as_deref().unwrap_or("?"),
    );
    let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

/// Log a request that returned early with an error (missing param, bad URL, etc.).
fn log_early_exit(method: &str, path: &str, reason: &str, ip: &Option<String>) {
    let ip_str = ip.as_deref().unwrap_or("?");
    let line = format!(
        "{} {} from {} - 400 {}\n",
        colour::cyan(method),
        path,
        ip_str,
        reason,
    );
    let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
}

/// Tiny colour helpers — duplicated here to avoid a circular dep on ux.
mod colour {
    pub(super) fn cyan(s: &str) -> String {
        if std::env::var("NO_COLOR").is_ok_and(|v| !v.is_empty()) { s.to_string() }
        else { format!("\x1b[36m{s}\x1b[0m") }
    }
}

// ── GET /placeholder/:kind.jpeg ───────────────────────────────────────────────

/// Serve the static placeholder thumbnail for a given file kind.
///
/// Kinds: `image`, `video`, `audio`, `vector`, `document`, `geometry`,
/// `archive`, `text`, `binary`, `unknown`, `failed`.
///
/// The `.jpeg` extension is required — `/placeholder/image.jpeg` works,
/// `/placeholder/image` does not.
///
/// These images are embedded at compile time and never change, so the
/// response includes aggressive cache headers.
pub async fn placeholder(
    axum::extract::Path(kind): axum::extract::Path<String>,
) -> Response {
    let Some(kind_name) = kind.strip_suffix(".jpeg") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let bytes: &'static [u8] = match kind_name {
        "image"    => crate::assets::placeholders::IMAGE,
        "video"    => crate::assets::placeholders::VIDEO,
        "audio"    => crate::assets::placeholders::AUDIO,
        "vector"   => crate::assets::placeholders::VECTOR,
        "document" => crate::assets::placeholders::DOCUMENT,
        "geometry" => crate::assets::placeholders::GEOMETRY,
        "archive"  => crate::assets::placeholders::ARCHIVE,
        "text"     => crate::assets::placeholders::TEXT,
        "binary"   => crate::assets::placeholders::BINARY,
        "unknown"  => crate::assets::placeholders::UNKNOWN,
        "failed"   => crate::assets::placeholders::FAILED,
        // Forward-compatible: any unrecognised kind silently falls back to
        // the generic "unknown" placeholder rather than 404-ing, so clients
        // that reference a kind added in a newer server release still get a
        // valid JPEG.
        _ => crate::assets::placeholders::UNKNOWN,
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        Bytes::from_static(bytes),
    )
        .into_response()
}

// ── GET /thumb ────────────────────────────────────────────────────────────────

/// Single-URL thumbnail endpoint.
///
/// # Request
///
/// ```text
/// GET /thumb.jpeg?url=http%3A%2F%2Fexample.com%2Fimage.jpg
/// ```
///
/// The `.jpeg` suffix on the path is the canonical form — it allows CDNs, social
/// media unfurlers, and image-aware middleware to identify the response as a JPEG
/// image from the URL alone without fetching it.  `/thumb` is an alias that maps
/// to the same handler for callers that prefer extension-free URLs.
///
/// This endpoint does not accept or return cache hints — use `/batch` for
/// conditional requests.
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
/// | 400    | JSON error          | Bad request (missing/bad URL)      |
/// | 404    | JSON error          | Source not found                   |
/// | 500    | JSON error          | Pipeline or upstream server error  |
#[derive(serde::Deserialize)]
pub struct ThumbQuery {
    #[serde(default)]
    pub url: Option<String>,
}

pub async fn thumb(
    State(runtime): State<Arc<Runtime>>,
    method: Method,
    Query(q): Query<ThumbQuery>,
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
) -> axum::response::Response {
    let t0 = Instant::now();
    let ip = client_ip(&headers, Some(&connect_info.0));
    let url_raw = q.url.unwrap_or_default();

    if url_raw.is_empty() {
        log_early_exit(method.as_str(), "/thumb.jpeg", "url parameter is required", &ip);
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "url parameter is required" })))
            .into_response();
    }
    let url = match normalize_url(url_raw, runtime.allow_local) {
        Ok(u)    => u,
        Err(msg) => {
            log_early_exit(method.as_str(), "/thumb.jpeg", msg, &ip);
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
        }
    };

    let input = InputSpec { url: url.clone(), cache: None, allow_local: runtime.allow_local };
    let (result, _trace, mut after) = ThumbCook::<PlatformStream>::from_input(input, runtime).run().await;
    after.drain_spawn();
    let duration_ms = t0.elapsed().as_millis() as u64;

    let media = result.media.as_ref();
    let _ux = ux::get();
    _ux.log_single_thumb(
        method.as_str(), "/thumb.jpeg", &url,
        200,
        duration_ms,
        media.and_then(|m| Some(kind_str(m.kind))),
        media.and_then(|m| Some(m.extension.as_str())),
        result.source.as_ref().map(source_label),
        result.message.as_deref(),
        ip.as_deref(),
    );

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
    connect_info: ConnectInfo<SocketAddr>,
    Json(req): Json<CallRequest>,
) -> Response {
    let _t0 = Instant::now();
    let stream_mode = wants_ndjson(&headers);
    let ip = client_ip(&headers, Some(&connect_info.0));

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

    // Log batch start.
    ux::get().log_batch_start("POST", "/batch", count, ip.as_deref());

    if stream_mode {
        // Streaming path: results logged from client side; server log is minimal.
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        let batch_runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let _ = tx.send(as_ndjson_line(json!({ "type": "batch.started", "count": count })));

            let mut pending = JoinSet::new();
            for (idx, spec) in jobs {
                let _ = tx.send(as_ndjson_line(json!({ "type": "item.accepted", "index": idx })));
                let item_runtime = Arc::clone(&batch_runtime);
                let item_tx = tx.clone();
                pending.spawn(async move {
                    let t_item = Instant::now();
                    let progress_tx = item_tx.clone();
                    let progress = Box::new(move |result| {
                        let _ = progress_tx.send(as_ndjson_line(json!({
                            "type": "item.intermediate", "index": idx, "result": result,
                        })));
                    });
                    let (result, _trace, mut after) = ThumbCook::<PlatformStream>::from_input(spec, item_runtime)
                        .run_with_progress(Some(progress)).await;
                    after.drain_spawn();
                    let dur = t_item.elapsed().as_millis() as u64;
                    log_result(&result, dur);
                    let _ = item_tx.send(as_ndjson_line(json!({
                        "type": "item.result", "index": idx, "result": result,
                    })));
                });
            }
            while pending.join_next().await.is_some() {}
            let _ = tx.send(as_ndjson_line(json!({ "type": "batch.complete" })));
        });

        let stream = stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|line| (Ok::<Bytes, Infallible>(line), rx))
        });
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/x-ndjson"), (header::CACHE_CONTROL, "no-store")],
            Body::from_stream(stream),
        ).into_response();
    }

    // Non-streaming: wait for all, then log each result.
    let mut pool = FuturesUnordered::new();
    for (idx, spec) in jobs {
        let job_runtime = Arc::clone(&runtime);
        pool.push(async move {
            let t_item = Instant::now();
            let (result, trace, after) = ThumbCook::<PlatformStream>::from_input(spec, job_runtime).run().await;
            let dur = t_item.elapsed().as_millis() as u64;
            (idx, result, trace, after, dur)
        });
    }

    let mut items = Vec::with_capacity(count);
    while let Some((_idx, result, _trace, mut after, dur)) = pool.next().await {
        after.drain_spawn();
        log_result(&result, dur);
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
/// When `allow_local` is `true` (`TBR_ALLOW_LOCAL=1`):
/// - `file://` URLs → accepted unchanged.
/// - Bare absolute paths (starting with `/`) → promoted to `file://` URLs
///   (e.g. `/data/img.png` becomes `file:///data/img.png`).
/// - Bare relative paths → rejected; the server's CWD is ambiguous.
fn normalize_url(url: String, allow_local: bool) -> Result<String, &'static str> {
    if url.contains("://") {
        if url.starts_with("file://") {
            if !allow_local {
                ux::warn_file_url_denied();
                return Err("file:// URLs are not permitted");
            }
            return Ok(url);
        }

        // Check for localhost / private-network hosts.
        if !allow_local {
            if let Ok(parsed) = url::Url::parse(&url) {
                if is_private_host(parsed.host_str()) {
                    ux::warn_localhost_denied();
                    return Err("localhost and private-network URLs are not permitted");
                }
            }
        }

        Ok(url)
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

fn is_private_host(host: Option<&str>) -> bool {
    let Some(h) = host else { return false };
    if h.eq_ignore_ascii_case("localhost")
        || h == "127.0.0.1"
        || h == "::1"
        || h == "[::1]"
    {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::Ipv4Addr>() {
        let octets = ip.octets();
        return octets[0] == 10
            || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
            || (octets[0] == 192 && octets[1] == 168)
            || octets[0] == 127;
    }
    false
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
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
    Json(payload): Json<ThumbHandoff>,
) -> axum::response::Response {
    let t0 = Instant::now();
    let ip = client_ip(&headers, Some(&connect_info.0));
    let url = payload.input.url.clone();

    // Handoff cooks do not own cache reads/writes or trace emission.
    // The entry tier is the authority for cache + trace in this request chain.
    let mut handoff_runtime = (*runtime).clone();
    handoff_runtime.cache = CacheStore::none();
    handoff_runtime.trace = TraceStore::none();
    let handoff_runtime = Arc::new(handoff_runtime);

    let (mut result, trace, mut after) = ThumbCook::<PlatformStream>::from_handoff(payload, handoff_runtime).run().await;
    after.drain_spawn();
    let duration_ms = t0.elapsed().as_millis() as u64;

    let media = result.media.as_ref();
    let _ux = ux::get();
    _ux.log_single_thumb(
        "POST", "/handoff", &url,
        if result.status == crate::result::ResultStatus::Success { 200 } else { 500 },
        duration_ms,
        media.and_then(|m| Some(kind_str(m.kind))),
        media.and_then(|m| Some(m.extension.as_str())),
        result.source.as_ref().map(source_label),
        result.message.as_deref(),
        ip.as_deref(),
    );

    // Save bandwidth on tier-to-tier responses: send placeholder token only.
    if result.media.as_ref().map_or(false, |m| !m.placeholder.is_empty()) {
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
/// ```text
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
