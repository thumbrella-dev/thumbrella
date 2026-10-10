//! Axum HTTP route handlers for the native server.
//!
//! Routes:
//! - `GET  /health`                       - liveness probe
//! - `GET  /placeholder/:kind.jpeg`       - static placeholder thumbnail for a file kind
//! - `GET  /thumb.jpeg?url=<url>`         - single thumbnail; returns raw JPEG bytes (canonical)
//! - `GET  /thumb?url=<url>`              - same handler; alias without extension
//! - `GET  /pin/:id.jpeg`                 - pinned JPEG or kind placeholder redirect
//! - `POST /handoff`                      - trusted tier-to-tier thumbnail handoff
//! - `POST /batch`                        - batch thumbnail + describe; waits for all items, returns one JSON object
//!
//! # Server token
//!
//! When `TBR_HANDSHAKE` is set, generation and service endpoints require the
//! `x-tbr-handshake` header. Pins and their placeholder redirects are public.

use std::{convert::Infallible, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use bytes::Bytes;
use futures::stream::{self, FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::{sync::mpsc, task::JoinSet};
use web_time::Instant;

use crate::cache::CacheStore;
use crate::cook::{InputSpec, Runtime, ThumbCook};
use crate::handoff::{HANDSHAKE_HEADER, HandoffResponse, ThumbHandoff};
use crate::http_buf::PlatformStream;
use crate::media::FileKind;
use crate::request::CallRequest;
use crate::result::{ResultSource, ThumbResult};
use crate::tracelog::TraceStore;
use crate::ux;

/// Maximum number of items accepted in a single `/batch` request.
/// Items beyond this limit are returned with `batchlimit` status.
const BATCH_MAX_ITEMS: usize = 12;

/// Maximum POST body size in bytes for the `/batch` endpoint.
/// Covers 12 items with generous URL, cache-string, and id fields.
pub const BATCH_MAX_BODY_BYTES: usize = 256 * 1024; // 256 KB

//  Helpers

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
        FileKind::Image => "image",
        FileKind::Video => "video",
        FileKind::Audio => "audio",
        FileKind::Vector => "vector",
        FileKind::Document => "document",
        FileKind::Geometry => "geometry",
        FileKind::Archive => "archive",
        FileKind::Text => "text",
        FileKind::Binary => "binary",
        FileKind::Unknown => "unknown",
    }
}

/// Return a human-readable label for a ResultSource.
fn source_label(source: &ResultSource) -> &'static str {
    match source {
        ResultSource::Render => "render",
        ResultSource::Cache => "cache",
        ResultSource::Shortcut => "shortcut",
        ResultSource::Placeholder => "placeholder",
        ResultSource::Fallback => "fallback",
        ResultSource::NotModified => "not_modified",
        ResultSource::Client => "client_error",
    }
}

/// Status code to report for a result in the terminal log.
///
/// Prefers the upstream status the pipeline captured (e.g. `304`), then maps
/// the result status.  A `not_modified` result with no upstream status means
/// the caller's cache token was still fresh, so nothing was re-made - it is
/// logged as `304` to match the revalidation path.
fn result_status_code(result: &crate::ThumbResult) -> u16 {
    if let Some(code) = result.http_status {
        return code;
    }
    if result.source == Some(ResultSource::NotModified) {
        return 304;
    }
    match result.status {
        crate::result::ResultStatus::Success => 200,
        crate::result::ResultStatus::Failed => 500,
        crate::result::ResultStatus::Overloaded => 503,
        crate::result::ResultStatus::Intermediate => 102,
        crate::result::ResultStatus::BatchLimit => 422,
    }
}

/// Log a completed thumbnail result through the UX layer.
fn log_result(result: &crate::ThumbResult, duration_ms: u64) {
    let ux = ux::get();
    let media = result.media.as_ref();
    ux.log_thumb_result(
        &result.url,
        result_status_code(result),
        duration_ms,
        media.map(|m| kind_str(m.kind)),
        media.map(|m| m.extension.as_str()),
        result.source.as_ref().map(source_label),
        result.message.as_deref(),
    );
}

//  GET /health

/// Liveness probe.  Returns server metadata:
///
/// ```json
/// {"status":"ok","thumbrella":0}
/// ```
///
/// The `thumbrella` field is the major version only - no minor or patch,
/// for safety against version-based targeting.
///
/// Logging is rate-limited: after 20 health checks, further requests are
/// suppressed and a one-time hint is printed.
pub async fn health(headers: HeaderMap, connect_info: ConnectInfo<SocketAddr>) -> Json<Value> {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    static HEALTH_COUNT: AtomicU32 = AtomicU32::new(0);
    static HINT_SHOWN: AtomicBool = AtomicBool::new(false);

    let full_log = matches!(std::env::var("TBR_LOG").as_deref(), Ok("full"));
    let n = HEALTH_COUNT.fetch_add(1, Ordering::Relaxed);
    let ip = client_ip(&headers, Some(&connect_info.0)).unwrap_or_else(|| "?".to_string());

    if full_log || n < 20 {
        ux::get().log_request("GET", "/health", 200, Some(&ip), None);
    } else if !HINT_SHOWN.swap(true, Ordering::Relaxed) {
        let line = "  # hint: no longer showing /health requests, use TBR_LOG=full to see them\n";
        let _ = std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes());
    }

    let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0);
    Json(json!({ "status": "ok", "thumbrella": major }))
}

//  Landing page

/// Returns a landing page at `/` with embedded logo and favicon.
///
/// When `TBR_HANDSHAKE` is set the thumbnail demo links are hidden (they
/// would fail without the handshake header) and the connect string includes
/// a placeholder for the secret.
pub async fn landing(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
) -> Response {
    let ip = client_ip(&headers, Some(&connect_info.0));
    ux::get().log_request("GET", "/", 200, ip.as_deref(), None);

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

//  Fallback - 404 for unknown routes

/// Catch-all for unmatched routes.  Logs the request and returns 404.
pub async fn not_found(
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
) -> Response {
    let ip = client_ip(&headers, Some(&connect_info.0));
    ux::get().log_request(method.as_str(), uri.path(), 404, ip.as_deref(), Some("not found"));
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

/// Log a request that returned early with an error (missing param, bad URL, etc.).
fn log_early_exit(method: &str, path: &str, reason: &str, ip: &Option<String>) {
    ux::get().log_request(method, path, 400, ip.as_deref(), Some(reason));
}

//  GET /placeholder/:kind.jpeg

/// Serve the static placeholder thumbnail for a given file kind.
///
/// Kinds: `image`, `video`, `audio`, `vector`, `document`, `geometry`,
/// `archive`, `text`, `binary`, `unknown`, `failed`.
///
/// The `.jpeg` extension is required - `/placeholder/image.jpeg` works,
/// `/placeholder/image` does not.
///
/// These images are embedded at compile time and never change, so the
/// response includes aggressive cache headers.
pub async fn placeholder(axum::extract::Path(kind): axum::extract::Path<String>, headers: HeaderMap) -> Response {
    let Some(kind_name) = kind.strip_suffix(".jpeg") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let asset = crate::assets::placeholder_asset(kind_name);
    let not_modified = asset.matches_if_none_match(
        headers.get_all(header::IF_NONE_MATCH).iter().filter_map(|value| value.to_str().ok()),
    );
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Bytes::from_static(asset.bytes).into_response()
    };
    response.headers_mut().insert(header::ETAG, asset.etag.parse().expect("hex ETag is a valid header"));
    response.headers_mut().insert(header::CACHE_CONTROL, crate::assets::PLACEHOLDER_CACHE_CONTROL.parse().unwrap());
    response.headers_mut().insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
    if !not_modified {
        response.headers_mut().insert(header::CONTENT_LENGTH, asset.bytes.len().into());
    }
    response
}

/// Public JPEG routes, kept outside the handshake-protected service router.
pub fn public_thumbnail_routes() -> axum::Router<Arc<Runtime>> {
    use axum::routing::get;
    let pins = axum::Router::new()
        .route("/pin", get(pin))
        .route("/pin/", get(pin))
        .route("/pin/{*filename}", get(pin))
        .layer(axum::middleware::from_fn(pin_minimum_runtime));
    pins.route("/placeholder/{kind}", get(placeholder))
        .layer(axum::middleware::from_fn(log_public_thumbnail_request))
}

fn has_pin_referrer(headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    let Some(referrer) = headers.get(header::REFERER).and_then(|value| value.to_str().ok())
        .and_then(|value| url::Url::parse(value).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
    else {
        return false;
    };
    if referrer.path() != "/pin" && !referrer.path().starts_with("/pin/") {
        return false;
    }
    let authority = uri.authority().map(|authority| authority.as_str())
        .or_else(|| headers.get(header::HOST).and_then(|value| value.to_str().ok()));
    let Some(origin) = authority.and_then(|authority| {
        url::Url::parse(&format!("{}://{authority}", referrer.scheme())).ok()
    }) else {
        return false;
    };
    origin.host_str() == referrer.host_str()
        && origin.port_or_known_default() == referrer.port_or_known_default()
}

async fn log_public_thumbnail_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let ip = client_ip(request.headers(), request.extensions()
        .get::<ConnectInfo<SocketAddr>>().map(|info| &info.0));
    let followed_pin = uri.path().starts_with("/placeholder/")
        && has_pin_referrer(request.headers(), &uri);
    let response = next.run(request).await;
    // Referer is only a best-effort log hint; it never affects the response.
    if !followed_pin || (!response.status().is_success() && response.status() != StatusCode::NOT_MODIFIED) {
        let redirect = response.headers().get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(|target| format!("redirect to {target}"));
        ux::get().log_request(method.as_str(), uri.path(), response.status().as_u16(),
            ip.as_deref(), redirect.as_deref());
    }
    response
}

const PIN_MINIMUM_RUNTIME: std::time::Duration = std::time::Duration::from_millis(10);

async fn pin_minimum_runtime(request: Request, next: Next) -> Response {
    let deadline = tokio::time::Instant::now() + PIN_MINIMUM_RUNTIME;
    let response = next.run(request).await;
    tokio::time::sleep_until(deadline).await;
    response
}

/// Resolve a pin without extending its retention. Well-formed missing, expired,
/// and disabled pins redirect to their kind; malformed paths return 404.
pub async fn pin(
    State(runtime): State<Arc<Runtime>>,
    filename: Result<axum::extract::Path<String>, axum::extract::rejection::PathRejection>,
    headers: HeaderMap,
) -> Response {
    use crate::cache::pins::{pin_kind, valid_pin};
    let Some(id) = filename.as_ref().ok()
        .and_then(|filename| filename.0.strip_suffix(".jpeg"))
        .filter(|id| valid_pin(id)) else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "error": "not found" })),
        ).into_response();
    };
    if runtime.pin_ttl_secs > 0 {
        match runtime.cache.pin_data(id).await {
            Ok(Some(pin)) if !pin.bytes.is_empty() => {
                let conditional = headers.contains_key(header::IF_NONE_MATCH).then(|| {
                    headers.get_all(header::IF_NONE_MATCH).iter()
                        .filter_map(|value| value.to_str().ok()).collect::<Vec<_>>().join(", ")
                });
                let since = headers.get(header::IF_MODIFIED_SINCE).and_then(|value| value.to_str().ok());
                let response = (|| -> Result<Response, String> {
                    let metadata = pin.headers(crate::cache::unix_now_secs(), conditional.as_deref(), since)?;
                    let length = pin.bytes.len();
                    let mut response = if metadata.not_modified {
                        StatusCode::NOT_MODIFIED.into_response()
                    } else {
                        Bytes::from(pin.bytes).into_response()
                    };
                    for (name, value) in [(header::ETAG, Some(metadata.etag)),
                        (header::LAST_MODIFIED, metadata.last_modified),
                        (header::CACHE_CONTROL, Some(metadata.cache_control))]
                    {
                        if let Some(value) = value {
                            response.headers_mut().insert(name, value.parse()
                                .map_err(|e| format!("invalid cached pin header: {e}"))?);
                        }
                    }
                    response.headers_mut().insert(header::CONTENT_TYPE, axum::http::HeaderValue::from_static("image/jpeg"));
                    if !metadata.not_modified { response.headers_mut().insert(header::CONTENT_LENGTH, length.into()); }
                    Ok(response)
                })();
                return match response {
                    Ok(response) => response,
                    Err(error) => {
                        tracing::warn!("pin HTTP metadata failed: {error}");
                        (StatusCode::INTERNAL_SERVER_ERROR, [(header::CACHE_CONTROL, "no-store")],
                            Json(json!({ "error": "pin metadata failed" }))).into_response()
                    }
                };
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!("pin lookup failed: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CACHE_CONTROL, "no-store")],
                    Json(json!({ "error": "pin lookup failed" })),
                ).into_response();
            }
        }
    }
    let kind = kind_str(pin_kind(id));
    let mut response = Redirect::temporary(&format!("/placeholder/{kind}.jpeg")).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(crate::http_cache::PIN_REDIRECT_CACHE_CONTROL));
    response
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use crate::cache::{CacheBackend, memory::MemoryCacheBackend, unix_now_secs};
    use crate::result::{ResultStatus, ThumbMedia};

    #[test]
    fn pin_referrer_detection_requires_same_server_and_a_pin_path() {
        let uri: axum::http::Uri = "/placeholder/image.jpeg".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "example.com:3114".parse().unwrap());
        for (referrer, expected) in [
            ("http://example.com:3114/pin/i123456789012.jpeg", true),
            ("http://example.com:3114/pin", true),
            ("http://example.com:3114/pin/malformed/extra", true),
            ("http://EXAMPLE.com:3114/pin/invalid", true),
            ("http://elsewhere.example:3114/pin/i123456789012.jpeg", false),
            ("http://example.com:3115/pin/i123456789012.jpeg", false),
            ("http://example.com:3114/pinning/image.jpeg", false),
            ("http://example.com:3114/", false),
            ("not a URL", false),
            ("file:///pin/i123456789012.jpeg", false),
        ] {
            headers.insert(header::REFERER, referrer.parse().unwrap());
            assert_eq!(has_pin_referrer(&headers, &uri), expected, "{referrer}");
        }
        headers.remove(header::REFERER);
        assert!(!has_pin_referrer(&headers, &uri));
        headers.insert(header::REFERER, "http://example.com:3114/pin/invalid".parse().unwrap());
        headers.remove(header::HOST);
        assert!(!has_pin_referrer(&headers, &uri));
        let absolute: axum::http::Uri = "http://example.com:3114/placeholder/image.jpeg".parse().unwrap();
        assert!(has_pin_referrer(&headers, &absolute));
    }

    fn runtime(cache: CacheStore, ttl: u64) -> Arc<Runtime> {
        let mut runtime = Runtime::new(cache, Default::default(), None, None,
            Default::default(), Default::default(), Some("pin-test-handshake".into()),
            false, 5, 60, 60, 1000, 60);
        Arc::make_mut(&mut runtime).pin_ttl_secs = ttl;
        runtime
    }

    async fn start_server(runtime: Arc<Runtime>) -> (String, tokio::task::JoinHandle<()>) {
        ux::init();
        let app = axum::Router::new()
            .route("/private", axum::routing::get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn_with_state(runtime.clone(), require_handshake))
            .merge(public_thumbnail_routes())
            .with_state(runtime);
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://127.0.0.1:{port}"), server)
    }

    async fn timed_get(client: &reqwest::Client, base: &str, path: &str) -> reqwest::Response {
        let start = web_time::Instant::now();
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert!(start.elapsed() >= PIN_MINIMUM_RUNTIME, "{path} returned before the pin minimum");
        response
    }

    #[tokio::test]
    async fn pinned_jpegs_use_cached_validators_and_freshness_without_renewing_pins() {
        use crate::cache::{chain::ChainCacheBackend, sqlite::SqliteCacheBackend};
        let backends: Vec<Arc<dyn CacheBackend>> = vec![
            Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap()),
            Arc::new(SqliteCacheBackend::open(":memory:").unwrap()),
            Arc::new(ChainCacheBackend::new(vec![
                Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap()),
                Arc::new(SqliteCacheBackend::open(":memory:").unwrap()),
            ])),
        ];
        for backend in backends {
            let now = unix_now_secs();
            let cache = crate::CacheHints {
                expires_at: Some(now + 3600), etag: Some("\"source-v1\"".into()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()), ..Default::default()
            }.encode(60);
            backend.put("source".into(), ThumbMedia {
                thumbnail: vec![1, 2, 3], cache: cache.clone(), ..Default::default()
            }, 0, now + 10, 600).await;
            backend.claim_pin("i123456789012", "source", 600).await.unwrap();
            let original = backend.pin_data("i123456789012").await.unwrap().unwrap();
            assert_eq!(original.cache, cache);
            let (base, server) = start_server(runtime(CacheStore::backend_only(backend.clone()), 600)).await;
            let client = reqwest::Client::builder().no_proxy()
                .redirect(reqwest::redirect::Policy::none()).build().unwrap();
            let path = "/pin/i123456789012.jpeg";
            let response = timed_get(&client, &base, path).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::ETAG], "\"source-v1\"");
            assert_eq!(response.headers()[header::LAST_MODIFIED], "Wed, 21 Oct 2015 07:28:00 GMT");
            let policy = response.headers()[header::CACHE_CONTROL].to_str().unwrap();
            let age: u64 = policy.strip_prefix("public, max-age=").unwrap().parse().unwrap();
            assert!((590..=600).contains(&age));
            assert_eq!(response.bytes().await.unwrap().as_ref(), &[1, 2, 3]);
            for (etag, since, status) in [
                (Some("\"source-v1\""), None, StatusCode::NOT_MODIFIED),
                (Some("W/\"source-v1\""), None, StatusCode::NOT_MODIFIED),
                (Some("\"other\", \"source-v1\""), None, StatusCode::NOT_MODIFIED),
                (Some("*"), None, StatusCode::NOT_MODIFIED),
                (None, Some("Wed, 21 Oct 2015 07:28:00 GMT"), StatusCode::NOT_MODIFIED),
                (None, Some("Thu, 22 Oct 2015 07:28:00 GMT"), StatusCode::NOT_MODIFIED),
                (None, Some("Tue, 20 Oct 2015 07:28:00 GMT"), StatusCode::OK),
                (Some("\"other\""), Some("Thu, 22 Oct 2015 07:28:00 GMT"), StatusCode::OK),
            ] {
                for method in [reqwest::Method::GET, reqwest::Method::HEAD] {
                    let mut request = client.request(method.clone(), format!("{base}{path}"));
                    if let Some(etag) = etag { request = request.header(header::IF_NONE_MATCH, etag); }
                    if let Some(since) = since { request = request.header(header::IF_MODIFIED_SINCE, since); }
                    let start = web_time::Instant::now();
                    let response = request.send().await.unwrap();
                    assert!(start.elapsed() >= PIN_MINIMUM_RUNTIME);
                    assert_eq!(response.status(), status);
                    assert_eq!(response.headers()[header::ETAG], "\"source-v1\"");
                    assert!(response.headers().contains_key(header::CACHE_CONTROL));
                    if status == StatusCode::NOT_MODIFIED || method == reqwest::Method::HEAD {
                        assert!(response.bytes().await.unwrap().is_empty());
                    }
                }
            }
            let redirect = client.get(format!("{base}/pin/i000000000000.jpeg"))
                .header(header::IF_NONE_MATCH, "*").send().await.unwrap();
            assert_eq!(redirect.status(), StatusCode::TEMPORARY_REDIRECT);
            assert!(!redirect.headers().contains_key(header::ETAG));
            assert_eq!(redirect.headers()[header::CACHE_CONTROL], "public, max-age=5");
            assert_eq!(backend.pin_data("i123456789012").await.unwrap().unwrap().until, original.until);
            server.abort();
        }
    }

    #[tokio::test]
    async fn placeholders_support_validators_conditional_get_and_head() {
        let (base, server) = start_server(runtime(CacheStore::none(), 0)).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for slug in ["image", "video", "audio", "vector", "document", "geometry",
            "archive", "text", "binary", "unknown", "failed", "future-kind"]
        {
            let url = format!("{base}/placeholder/{slug}.jpeg");
            let asset = crate::assets::placeholder_asset(slug);
            let response = client.get(&url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::ETAG], asset.etag);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], asset.bytes.len().to_string());
            assert_eq!(response.headers()[header::CACHE_CONTROL], crate::assets::PLACEHOLDER_CACHE_CONTROL);
            if let Some(length) = response.headers().get(header::CONTENT_LENGTH) {
                assert_eq!(length.to_str().unwrap(), asset.bytes.len().to_string());
            }
            assert!(!response.headers().contains_key(header::LAST_MODIFIED));
            assert_eq!(response.bytes().await.unwrap().as_ref(), asset.bytes);
            for validator in [asset.etag.to_string(), format!("W/{}", asset.etag),
                format!("\"stale\", {}", asset.etag), "*".into()]
            {
                for method in [reqwest::Method::GET, reqwest::Method::HEAD] {
                    let response = client.request(method, &url)
                        .header(header::IF_NONE_MATCH, &validator).send().await.unwrap();
                    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
                    assert_eq!(response.headers()[header::ETAG], asset.etag);
                    assert_eq!(response.headers()[header::CACHE_CONTROL], crate::assets::PLACEHOLDER_CACHE_CONTROL);
                    assert!(response.bytes().await.unwrap().is_empty());
                }
            }
            let response = client.head(&url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], asset.bytes.len().to_string());
            assert!(response.bytes().await.unwrap().is_empty());
            let response = client.get(&url).header(header::IF_NONE_MATCH, "\"stale\"")
                .header(header::IF_MODIFIED_SINCE, "Wed, 31 Dec 2099 00:00:00 GMT").send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap().as_ref(), asset.bytes);
        }
        assert_eq!(client.get(format!("{base}/placeholder/image"))
            .header(header::IF_NONE_MATCH, "*").send().await.unwrap().status(), StatusCode::NOT_FOUND);
        let no_redirect = reqwest::Client::builder().no_proxy()
            .redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let response = no_redirect.get(format!("{base}/pin/i000000000000.jpeg"))
            .header(header::IF_NONE_MATCH, "*").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        server.abort();
    }

    #[tokio::test]
    async fn public_pin_routes_delay_hits_misses_and_malformed_paths_and_allow_redirects_without_handshake() {
        let backend = Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap());
        backend.put("source".into(), ThumbMedia {
            thumbnail: vec![0xff, 0xd8, 0xff, 0xd9], ..Default::default()
        }, 0, unix_now_secs() + 60, 60).await;
        backend.claim_pin("i123456789012", "source", 60).await.unwrap();
        let (base, server) = start_server(runtime(CacheStore::backend_only(backend.clone()), 60)).await;
        let client = reqwest::Client::builder().no_proxy()
            .redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let response = timed_get(&client, &base, "/pin/i123456789012.jpeg").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.bytes().await.unwrap().as_ref(), &[0xff, 0xd8, 0xff, 0xd9]);
        // Even an incorrect handshake must not block a bearer pin.
        assert_eq!(client.get(format!("{base}/pin/i123456789012.jpeg"))
            .header(HANDSHAKE_HEADER, "incorrect").send().await.unwrap().status(), StatusCode::OK);
        for (path, target) in [("/pin/v000000000000.jpeg", "/placeholder/video.jpeg")] {
            let response = timed_get(&client, &base, path).await;
            assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT, "{path}");
            assert_eq!(response.headers()[header::LOCATION], target, "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=5");
            assert!(!response.headers().contains_key(header::ETAG));
            let placeholder = client.get(format!("{base}{target}")).send().await.unwrap();
            assert_eq!(placeholder.status(), StatusCode::OK);
            assert_eq!(placeholder.headers()[header::CONTENT_TYPE], "image/jpeg");
        }
        for path in ["/pin", "/pin/", "/pin/g-invalid.jpeg", "/pin/x123456789012.jpeg",
            "/pin/i123456789012.png", "/pin/i123456789012.jpeg/extra", "/pin/%FF.jpeg", "/pin/%2F.jpeg"]
        {
            let response = timed_get(&client, &base, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(!response.headers().contains_key(header::LOCATION), "{path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.json::<Value>().await.unwrap()["error"], "not found");
        }
        assert_eq!(client.get(format!("{base}/private")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(client.get(format!("{base}/private")).header(HANDSHAKE_HEADER, "pin-test-handshake")
            .send().await.unwrap().status(), StatusCode::OK);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());

        let (base, server) = start_server(runtime(CacheStore::backend_only(backend), 0)).await;
        let response = timed_get(&client, &base, "/pin/i123456789012.jpeg").await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], "/placeholder/image.jpeg");
        let malformed = timed_get(&client, &base, "/pin/garbage").await;
        assert_eq!(malformed.status(), StatusCode::NOT_FOUND);
        assert!(!malformed.headers().contains_key(header::LOCATION));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    struct FailingPinBackend;

    impl CacheBackend for FailingPinBackend {
        fn name(&self) -> &'static str { "failing-pin-test" }
        fn get_entry<'a>(&'a self, _: &'a str, _: u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<crate::cache::CacheEntry>> + Send + 'a>> {
            panic!("pin lookup must not use ordinary entry lookup")
        }
        fn get_pin_entry<'a>(&'a self, _: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<crate::cache::CacheEntry>> + Send + 'a>> {
            panic!("pin route must only fetch thumbnail bytes")
        }
        fn pin_data<'a>(&'a self, _: &'a str) -> crate::cache::PinFuture<'a, Option<crate::http_cache::PinnedThumbnail>> {
            Box::pin(async { Err("test backend lookup failed".into()) })
        }
        fn promote(&self, _: String, _: crate::cache::CacheEntry) -> crate::after::DeferredFuture {
            panic!("pin lookup must not write")
        }
        fn revalidate(&self, _: String, _: crate::cache::CacheEntry, _: String) -> crate::after::DeferredFuture {
            panic!("pin lookup must not revalidate")
        }
        fn put(&self, _: String, _: ThumbMedia, _: u8, _: u64, _: u64) -> crate::after::DeferredFuture {
            panic!("pin lookup must not write")
        }
    }

    #[tokio::test]
    async fn untracked_placeholder_pin_redirects_until_a_render_activates_the_same_url() {
        let backend = Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap());
        let store = CacheStore::backend_only(backend.clone());
        let mut result = ThumbResult {
            status: ResultStatus::Success,
            source: Some(ResultSource::Placeholder),
            media: Some(ThumbMedia { kind: FileKind::Image, placeholder: "image".into(), ..Default::default() }),
            ..Default::default()
        };
        let url = store.pin_result("source", &result, 60).await.unwrap().unwrap();
        let (base, server) = start_server(runtime(store.clone(), 60)).await;
        let client = reqwest::Client::builder().no_proxy()
            .redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let response = timed_get(&client, &base, &format!("/{url}")).await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], "/placeholder/image.jpeg");
        assert!(backend.get_entry("source", 0).await.is_none());
        assert!(backend.get_pin_entry("source").await.is_none());
        let media = result.media.as_mut().unwrap();
        media.placeholder.clear();
        media.thumbnail = vec![0xff, 0xd8, 0xff, 0xd9];
        backend.put("source".into(), media.clone(), 0, unix_now_secs() + 60, 0).await;
        result.source = Some(ResultSource::Render);
        assert_eq!(store.pin_result("source", &result, 60).await.unwrap().as_deref(), Some(url.as_str()));
        let response = timed_get(&client, &base, &format!("/{url}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), &[0xff, 0xd8, 0xff, 0xd9]);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn pin_backend_errors_are_delayed_and_stay_explicit_errors() {
        let (base, server) = start_server(runtime(CacheStore::backend_only(Arc::new(FailingPinBackend)), 60)).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = timed_get(&client, &base, "/pin/i123456789012.jpeg").await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.json::<Value>().await.unwrap()["error"], "pin lookup failed");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn pin_route_returns_only_jpeg_and_redirects_missing_or_disabled_pins() {
        let backend = Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap());
        let cache = CacheStore::backend_only(backend.clone());
        let media = ThumbMedia {
            thumbnail: vec![0xff, 0xd8, 0xff, 0xd9],
            kind: FileKind::Image,
            ..Default::default()
        };
        backend.put("source".into(), media.clone(), 0, unix_now_secs() + 60, 60).await;
        let result = ThumbResult { status: ResultStatus::Success, media: Some(media.clone()), ..Default::default() };
        let url = cache.pin_result("source", &result, 60).await.unwrap().unwrap();
        let id = url.strip_prefix("pin/").unwrap().strip_suffix(".jpeg").unwrap();
        let mut runtime = Runtime::new(cache, Default::default(), None, None,
            Default::default(), Default::default(), None, false, 5, 60, 60, 1000, 60);
        Arc::make_mut(&mut runtime).pin_ttl_secs = 60;
        let before = backend.get_pin_entry("source").await.unwrap().pin_until;
        let response = pin(State(runtime.clone()), Ok(axum::extract::Path(format!("{id}.jpeg"))), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(axum::body::to_bytes(response.into_body(), 100).await.unwrap().as_ref(), media.thumbnail);
        assert_eq!(backend.get_pin_entry("source").await.unwrap().pin_until, before);
        for (invalid, kind) in [("v123456789012.jpeg", "video")] {
            let response = pin(State(runtime.clone()), Ok(axum::extract::Path(invalid.into())), HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
            assert_eq!(response.headers()[header::LOCATION], format!("/placeholder/{kind}.jpeg"));
            assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=5");
            assert!(!response.headers().contains_key(header::ETAG));
        }
        for invalid in ["g-invalid.jpeg", "invalid", "x"] {
            let response = pin(State(runtime.clone()), Ok(axum::extract::Path(invalid.into())), HeaderMap::new()).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        Arc::make_mut(&mut runtime).pin_ttl_secs = 0;
        let response = pin(State(runtime), Ok(axum::extract::Path(format!("{id}.jpeg"))), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], "/placeholder/image.jpeg");
    }
}

/// Single-URL thumbnail endpoint.
///
/// # Request
///
/// ```text
/// GET /thumb.jpeg?url=http%3A%2F%2Fexample.com%2Fimage.jpg
/// ```
///
/// The `.jpeg` suffix on the path is the canonical form - it allows CDNs, social
/// media unfurlers, and image-aware middleware to identify the response as a JPEG
/// image from the URL alone without fetching it.  `/thumb` is an alias that maps
/// to the same handler for callers that prefer extension-free URLs.
///
/// This endpoint does not accept or return cache hints - use `/batch` for
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
    let url = match normalize_url(url_raw.clone(), runtime.allow_local) {
        Ok(u) => u,
        Err(msg) => {
            log_early_exit(
                method.as_str(),
                "/thumb.jpeg",
                &format!("{msg} ({url_raw})"),
                &ip,
            );
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
        }
    };

    let input = InputSpec {
        url: url.clone(),
        cache: None,
        allow_local: runtime.allow_local,
    };
    let (result, _trace, mut after) = ThumbCook::<PlatformStream>::from_input(input, runtime).run().await;
    after.drain_spawn();
    let duration_ms = t0.elapsed().as_millis() as u64;

    let media = result.media.as_ref();
    let _ux = ux::get();
    _ux.log_single_thumb(
        method.as_str(),
        "/thumb.jpeg",
        &url,
        result_status_code(&result),
        duration_ms,
        media.map(|m| kind_str(m.kind)),
        media.map(|m| m.extension.as_str()),
        result.source.as_ref().map(source_label),
        result.message.as_deref(),
        ip.as_deref(),
    );

    let thumb = result.media.as_ref().map(|m| &m.thumbnail);
    if thumb.is_none_or(|t| t.is_empty()) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": result.message.unwrap_or_default() })),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/jpeg")],
        Bytes::from(thumb.unwrap().clone()),
    )
        .into_response()
}

//  POST /batch

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
        let url = match normalize_url(url.clone(), runtime.allow_local) {
            Ok(u) => u,
            Err(msg) => {
                // One rejected item aborts the whole batch - no item is
                // processed, so report what was refused and why.
                log_early_exit("POST", "/batch", &format!("{msg} ({url})"), &ip);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("items[{idx}]: {msg}") })),
                )
                    .into_response();
            }
        };
        jobs.push((
            idx,
            InputSpec {
                url: url.clone(),
                cache,
                allow_local: runtime.allow_local,
            },
        ));
    }

    // Enforce per-request batch limit.  Items beyond the limit are returned
    // with `batchlimit` status so clients can retry them in a smaller batch.
    let overflow: Vec<ThumbResult> = if jobs.len() > BATCH_MAX_ITEMS {
        jobs.split_off(BATCH_MAX_ITEMS)
            .into_iter()
            .map(|(_, spec)| ThumbResult::batch_limit(spec.url, BATCH_MAX_ITEMS))
            .collect()
    } else {
        Vec::new()
    };
    let count = jobs.len() + overflow.len();

    // Log batch start.
    ux::get().log_batch_start("POST", "/batch", count, ip.as_deref());

    if stream_mode {
        // Streaming path: results logged from client side; server log is minimal.
        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        let batch_runtime = Arc::clone(&runtime);
        // Emit overflow batchlimit results immediately.
        for result in &overflow {
            let _ = tx.send(as_ndjson_line(
                serde_json::to_value(result).unwrap_or_default(),
            ));
        }
        tokio::spawn(async move {
            let mut pending = JoinSet::new();
            for (_idx, spec) in jobs {
                let item_runtime = Arc::clone(&batch_runtime);
                let item_tx = tx.clone();
                pending.spawn(async move {
                    let t_item = Instant::now();
                    let progress_tx = item_tx.clone();
                    let progress = Box::new(move |result| {
                        let _ = progress_tx.send(as_ndjson_line(
                            serde_json::to_value(&result).unwrap_or_default(),
                        ));
                    });
                    let (result, _trace, mut after) =
                        ThumbCook::<PlatformStream>::from_input(spec, item_runtime)
                            .run_with_progress(Some(progress))
                            .await;
                    after.drain_spawn();
                    let dur = t_item.elapsed().as_millis() as u64;
                    log_result(&result, dur);
                    let _ = item_tx.send(as_ndjson_line(
                        serde_json::to_value(&result).unwrap_or_default(),
                    ));
                });
            }
            while pending.join_next().await.is_some() {}
        });

        let stream = stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|line| (Ok::<Bytes, Infallible>(line), rx))
        });
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-ndjson"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            Body::from_stream(stream),
        )
            .into_response();
    }

    // Non-streaming: wait for all, then log each result.
    let mut pool = FuturesUnordered::new();
    for (idx, spec) in jobs {
        let job_runtime = Arc::clone(&runtime);
        pool.push(async move {
            let t_item = Instant::now();
            let (result, trace, after) =
                ThumbCook::<PlatformStream>::from_input(spec, job_runtime).run().await;
            let dur = t_item.elapsed().as_millis() as u64;
            (idx, result, trace, after, dur)
        });
    }

    let mut items = Vec::with_capacity(count);
    // Prepend batchlimit overflow results.
    items.extend(overflow);
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
/// When `allow_local` is `true` (`TBR_LOCAL=1`):
/// - `file://` URLs → accepted unchanged.
/// - Bare absolute paths → promoted to `file://` URLs (e.g. `/data/img.png`
///   becomes `file:///data/img.png`, and on Windows `C:/data/img.png` becomes
///   `file:///C:/data/img.png`).  See [`crate::local_path`].
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
        if !allow_local
            && let Ok(parsed) = url::Url::parse(&url)
            && is_private_host(parsed.host_str())
        {
            ux::warn_localhost_denied();
            return Err("localhost and private-network URLs are not permitted");
        }

        Ok(url)
    } else if allow_local {
        if let Some(path) = crate::local_path::path_for_file_url(&url) {
            Ok(format!("file://{path}"))
        } else {
            Err("bare relative paths are not permitted; use an absolute path or a file:// URL")
        }
    } else {
        Err("url must be an http:// or https:// URL")
    }
}

fn is_private_host(host: Option<&str>) -> bool {
    let Some(h) = host else {
        return false;
    };
    if h.eq_ignore_ascii_case("localhost") || h == "127.0.0.1" || h == "::1" || h == "[::1]" {
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
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|accept| {
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

//  POST /handoff

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

    let (mut result, trace, mut after) =
        ThumbCook::<PlatformStream>::from_handoff(payload, handoff_runtime).run().await;
    after.drain_spawn();
    let duration_ms = t0.elapsed().as_millis() as u64;

    let media = result.media.as_ref();
    let _ux = ux::get();
    _ux.log_single_thumb(
        "POST",
        "/handoff",
        &url,
        if result.status == crate::result::ResultStatus::Success { 200 } else { 500 },
        duration_ms,
        media.map(|m| kind_str(m.kind)),
        media.map(|m| m.extension.as_str()),
        result.source.as_ref().map(source_label),
        result.message.as_deref(),
        ip.as_deref(),
    );

    // Save bandwidth on tier-to-tier responses: send placeholder token only.
    if result.media.as_ref().is_some_and(|m| !m.placeholder.is_empty())
        && let Some(ref mut media) = result.media
    {
        media.thumbnail.clear();
    }

    let body = HandoffResponse { result, trace };
    (StatusCode::OK, Json(body)).into_response()
}

//  Server-token middleware

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

    let provided = headers.get(HANDSHAKE_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");

    if provided != expected {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "invalid handshake"}))).into_response();
    }

    next.run(request).await
}
