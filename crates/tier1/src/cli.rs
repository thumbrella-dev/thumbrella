//! Native CLI / server entry point.
//!
//! Shared between the `tier1` and `tier2` binaries.  Each binary's `main.rs`
//! is a minimal stub that calls [`run`].
//!
//! ```text
//! <binary> serve              # start the HTTP server
//! <binary> thumb <url>...     # thumbnail one or more URLs
//! <binary> render <src> <dst> # thumbnail a local file to disk
//! <binary> diag               # print config and validate services
//! ```

use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::cook::Runtime;

// ── CLI schema ────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Thumbrella — thumbnail and describe service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    ///
    /// Port and other options come from environment variables (defaults).
    /// TBR_PORT (3114) serve port
    /// TBR_HANDSHAKE shared secret required on all endpoints (when set)
    /// TBR_TIER2 downstream tier2 connect string (URL + optional comma-separated headers)
    /// TBR_TIER3 downstream tier3 connect string (URL + optional comma-separated headers)
    Serve,

    /// Thumbnail one or more URLs and print results to stdout.
    ///
    /// All URLs are processed concurrently.  Output is a JSON object with an
    /// `items` array, one `ThumbResult` per input URL — the same shape as the
    /// `/batch` endpoint response.
    Thumb {
        /// Source URLs to thumbnail.
        #[arg(required = true)]
        urls: Vec<String>,

        /// Previously returned cache hints JSON (from `ThumbResult.cache`).
        ///
        /// When supplied, enables conditional fetch and client-side freshness
        /// checks.  Pass the value of the `cache` field from a prior result.
        #[arg(long)]
        cache: Option<String>,

        /// Emit machine-readable JSON instead of the default pretty text.
        #[arg(long)]
        json: bool,

        /// Include full base64 thumbnail data in JSON output.
        /// Without this flag, large thumbnails are replaced with a placeholder.
        #[arg(long)]
        raw: bool,

        /// Show internal trace fields (download metrics, render path, IDs, …).
        #[arg(long)]
        trace: bool,
    },

    /// Render a single local file to a thumbnail JPEG and write it to disk.
    ///
    /// Input must be a local path or `file://` URL.  Output is the path where
    /// the 250×200 JPEG thumbnail will be written.  Local filesystem access
    /// is enabled automatically for this command.
    Render {
        /// Path to the source file to thumbnail.
        src: String,

        /// Path where the thumbnail JPEG will be written.
        dst: String,
    },

    /// Print server configuration and validate connected services.
    ///
    /// Reports tier status, cache config, account credentials, and concurrency
    /// limits.  Validates external dependencies (handoff servers, caches) where
    /// possible.  Output is private — not exposed on any HTTP endpoint.
    Diag {
        /// Emit machine-readable JSON instead of the default pretty text.
        #[arg(long)]
        json: bool,
    },

    /// Submit a batch request in streaming mode and print result events as they arrive.
    ///
    /// Sends `Accept: application/x-ndjson` and prints one JSON line per
    /// `item.result` event with client-measured elapsed milliseconds since submit.
    #[command(name = "stream-batch")]
    StreamBatch {
        /// Tier server base URL.
        ///
        /// Can be supplied either as `--server <url>` or as the first
        /// positional argument before the thumbnail URLs.
        #[arg(long)]
        server: Option<String>,

        /// Source URLs to include in the batch.
        #[arg(required = true, num_args = 1..)]
        args: Vec<String>,

        /// Previously returned cache hints JSON (from `ThumbResult.cache`).
        #[arg(long)]
        cache: Option<String>,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Initialise logging, parse arguments, and run the selected command.
///
/// Intended to be called directly from `#[tokio::main] async fn main()`.
pub async fn run() {
    run_with_hook(|rt| async { rt }).await;
}

/// Like [`run`], but allows the caller to inspect or modify the [`Runtime`]
/// immediately after startup, before any command is dispatched.
///
/// The `hook` receives the freshly constructed `Arc<Runtime>` and must return
/// an `Arc<Runtime>` (possibly the same one, possibly a new one built with
/// [`crate::renderer::with_renderer`]).
///
/// # Example — tier 2 binary
/// ```ignore
/// tier1::cli::run_with_hook(|rt| async move {
///     tier1::with_renderer(rt, std::sync::Arc::new(tier2::Tier2Renderer::new()))
/// }).await;
/// ```
pub async fn run_with_hook<F, Fut>(hook: F)
where
    F: FnOnce(Arc<Runtime>) -> Fut,
    Fut: std::future::Future<Output = Arc<Runtime>>,
{
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    let runtime = if !matches!(cli.command, Command::Diag { .. } | Command::StreamBatch { .. }) {
        let cfg = crate::config::AppConfig::from_env();
        let rt = crate::startup::startup(&cfg).await;
        Some(hook(rt).await)
    } else {
        None
    };

    match cli.command {
        Command::Serve                                   => run_server(runtime.unwrap()).await,
        Command::Thumb { urls, cache, json, raw, trace } => run_thumb(urls, cache, json, raw, trace, runtime.unwrap()).await,
        Command::Render { src, dst }                     => run_render(src, dst, runtime.unwrap()).await,
        Command::Diag { json }                           => run_diag(json),
        Command::StreamBatch { server, args, cache } => {
            let (server, urls) = normalize_stream_batch_args(server, args);
            run_stream_batch(server, urls, cache).await;
        }
    }
}

fn normalize_stream_batch_args(server: Option<String>, mut args: Vec<String>) -> (String, Vec<String>) {
    const DEFAULT_SERVER: &str = "http://127.0.0.1:8001";

    if let Some(server) = server {
        return (server, args);
    }

    if args.len() >= 2 && looks_like_server_base(&args[0]) {
        let server = args.remove(0);
        return (server, args);
    }

    (DEFAULT_SERVER.to_string(), args)
}

fn looks_like_server_base(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };

    if url.query().is_some() || url.fragment().is_some() {
        return false;
    }

    matches!(url.path(), "" | "/")
}

// ── serve ─────────────────────────────────────────────────────────────────────

async fn run_server(runtime: Arc<Runtime>) {
    use axum::{Router, routing::{get, post}};
    use std::net::SocketAddr;
    use crate::{config::AppConfig, routes};

    let cfg = AppConfig::from_env();
    tracing::info!(version = crate::TBR_VERSION, "thumbrella starting");

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/placeholder/{kind}", get(routes::placeholder))
        .route("/thumb.jpeg", get(routes::thumb))
        .route("/thumb", get(routes::thumb))
        .route("/handoff", post(routes::handoff))
        .route("/batch", post(routes::batch))
        .layer(axum::middleware::from_fn_with_state(
            runtime.clone(),
            routes::require_handshake,
        ))
        .with_state(runtime);

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));
    tracing::info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

/// Wait for a shutdown signal (SIGTERM or SIGINT).
///
/// On Unix, SIGTERM is sent by `docker stop` and container orchestrators.
/// SIGINT is sent by Ctrl+C in a local terminal.  This future resolves
/// when either is received, allowing the server to drain in-flight requests
/// and shut down cleanly instead of being force-killed after the Docker
/// stop timeout.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
        tracing::info!("received SIGINT, shutting down");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
        tracing::info!("received SIGTERM, shutting down");
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, draining connections");
}

// ── thumb (CLI) ───────────────────────────────────────────────────────────────

/// Promote a bare filesystem path to a `file://` URL.
///
/// Paths that already have a scheme (`http://`, `https://`, `file://`) are
/// returned unchanged.  Relative paths are resolved against the current
/// working directory.
pub fn promote_url(raw: &str) -> String {
    if raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw.starts_with("file://")
    {
        return raw.to_string();
    }
    let path = std::path::Path::new(raw);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };
    format!("file://{}", abs.display())
}

async fn run_thumb(urls: Vec<String>, cache_str: Option<String>, json: bool, raw: bool, show_trace: bool, runtime: Arc<Runtime>) {
    use futures::stream::{FuturesUnordered, StreamExt};
    use crate::{ThumbCook, cook::InputSpec};
    use crate::source::CacheHints;

    let cache = cache_str.as_deref().and_then(CacheHints::decode);

    let mut pool = FuturesUnordered::new();
    for raw in urls {
        let is_local = !raw.contains("://") || raw.starts_with("file://");
        let url = promote_url(&raw);
        let input = InputSpec { url, cache: cache.clone(), allow_local: is_local };
        pool.push(ThumbCook::from_input(input, Arc::clone(&runtime)).run());
    }

    let mut items: Vec<(crate::ThumbResult, crate::ThumbTrace)> = Vec::with_capacity(pool.len());
    while let Some((result, trace, mut after)) = pool.next().await {
        after.drain_spawn();
        items.push((result, trace));
    }

    if json {
        let json_items: Vec<serde_json::Value> = items.iter().map(|(result, trace)| {
            let mut result_val = serde_json::to_value(result).unwrap();
            if !raw {
                if let Some(obj) = result_val.as_object_mut() {
                    if let Some(media) = obj.get_mut("media").and_then(|m| m.as_object_mut()) {
                        if let Some(thumb) = media.get("thumbnail") {
                            if thumb.as_str().is_some_and(|s| !s.is_empty()) {
                                media.insert("thumbnail".into(), serde_json::Value::String("<binary image data>".into()));
                            }
                        }
                    }
                }
            }
            serde_json::json!({ "result": result_val, "trace": trace })
        }).collect();
        let out = serde_json::json!({ "items": json_items });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        print_thumb_items(&items, show_trace);
    }
}

// ── render (CLI) ──────────────────────────────────────────────────────────────

async fn run_render(src: String, dst: String, runtime: Arc<Runtime>) {
    use crate::{ThumbCook, cook::InputSpec};

    let url = promote_url(&src);
    let input = InputSpec { url, cache: None, allow_local: true };
    let (result, _trace, mut after) = ThumbCook::from_input(input, runtime).run().await;
    after.drain_spawn();

    let thumb_empty = result.media.as_ref().map_or(true, |m| m.thumbnail.is_empty());
    if thumb_empty {
        let reason = result.message.as_deref().filter(|m| !m.is_empty());
        if let Some(msg) = reason {
            eprintln!("render: no thumbnail produced ({msg})");
        } else {
            eprintln!("render: no thumbnail produced");
        }
        return;
    }

    let thumb = &result.media.as_ref().unwrap().thumbnail;
    if let Err(e) = std::fs::write(&dst, thumb) {
        eprintln!("render: failed to write {}: {e}", dst);
        return;
    }

    println!("wrote {} bytes to {}", thumb.len(), dst);
}

async fn run_stream_batch(server: String, urls: Vec<String>, cache: Option<String>) {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use futures::StreamExt;
    use reqwest::header;
    use tokio::time::Instant;

    let endpoint = format!("{}/batch", server.trim_end_matches('/'));
    let request = crate::CallRequest {
        items: urls
            .into_iter()
            .map(|url| {
                if let Some(ref c) = cache {
                    crate::ThumbInput::Object(crate::ThumbObject {
                        url,
                        cache: Some(c.clone()),
                    })
                } else {
                    crate::ThumbInput::Url(url)
                }
            })
            .collect(),
    };

    let started = Instant::now();
    let response = match reqwest::Client::new()
        .post(endpoint)
        .header(header::ACCEPT, "application/x-ndjson")
        .json(&request)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            eprintln!("stream request failed: {err}");
            return;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        eprintln!("stream request failed with {status}: {body}");
        return;
    }

    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else {
            eprintln!("stream read failed");
            return;
        };
        pending.extend_from_slice(&bytes);

        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let mut line = pending.drain(..=pos).collect::<Vec<u8>>();
            while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
                continue;
            };
            let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let is_result       = event_type == "item.result";
            let is_intermediate = event_type == "item.intermediate";
            if !is_result && !is_intermediate {
                continue;
            }
            let Some(result) = value.get("result") else {
                continue;
            };

            let mut result = result.clone();
            if let Some(obj) = result.as_object_mut() {
                if let Some(thumb) = obj.get("thumbnail").and_then(|v| v.as_str()) {
                    if !thumb.is_empty() {
                        let kb = STANDARD
                            .decode(thumb)
                            .ok()
                            .map(|bytes| bytes.len().div_ceil(1024))
                            .unwrap_or(0);
                        obj.insert(
                            "thumbnail".into(),
                            serde_json::Value::String(format!("<binary thumbnail data {kb} kb>")),
                        );
                    }
                }
            }

            let out = serde_json::json!({
                "elapsed_ms": started.elapsed().as_millis(),
                "event":      if is_intermediate { "loading" } else { "result" },
                "result": result,
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_string()));
        }
    }
}

// ── thumb pretty printer ──────────────────────────────────────────────────────

pub fn print_thumb_items(items: &[(crate::ThumbResult, crate::ThumbTrace)], show_trace: bool) {
    for (result, trace) in items {
        let result_json = serde_json::to_value(result).unwrap_or_default();
        let trace_json  = serde_json::to_value(trace).unwrap_or_default();
        let get_str = |obj: &serde_json::Value, key: &str| -> String {
            obj.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace('_', " ")
        };

        let url_display = result.url.as_str();
        let sep_len = (56usize).saturating_sub(url_display.len() + 4);
        let sep = "─".repeat(sep_len.max(4));
        println!("── {url_display} {sep}");

        let status = get_str(&result_json, "status");
        let status_str = if let Some(ref msg) = result.message {
            format!("{status}  ({msg})")
        } else {
            status
        };
        println!("  status    : {status_str}");

        {
            let kind = get_str(&result_json, "kind");
            let media = result.media.as_ref();
            let ext  = media.map(|m| m.extension.as_str()).unwrap_or("");
            let mime = media.map(|m| m.mime.as_str()).unwrap_or("");
            let size = media.map(|m| fmt_bytes(m.file_size)).unwrap_or_default();
            let parts: Vec<&str> = [kind.as_str(), ext, mime, size.as_str()]
                .iter().copied().filter(|s| !s.is_empty()).collect();
            if !parts.is_empty() {
                println!("  format    : {}", parts.join("  "));
            }
        }

        if let Some(ref media) = result.media {
            if let Some(obj) = media.properties.as_object() {
                let pairs: Vec<String> = obj.iter()
                    .map(|(k, v)| {
                        if let Some(s) = v.as_str() { format!("{k}={s}") }
                        else { format!("{k}={v}") }
                    })
                    .collect();
                if !pairs.is_empty() {
                    println!("  properties: {}", pairs.join("  "));
                }
            }
        }

        if let Some(ref media) = result.media {
            if !media.thumbnail.is_empty() {
                println!("  thumbnail : <binary image data>  (250×200  {})",
                    fmt_bytes(media.thumbnail.len() as u64));
            }
        }

        if let Some(ref cache_str) = result.media.as_ref().and_then(|m| m.cache.as_deref()) {
            if let Some(hints) = crate::source::CacheHints::decode(cache_str) {
                if let Ok(val) = serde_json::to_value(&hints) {
                    if let Some(obj) = val.as_object() {
                        let pairs: Vec<String> = obj.iter()
                            .map(|(k, v)| {
                                if let Some(s) = v.as_str() { format!("{k}={s}") }
                                else { format!("{k}={v}") }
                            })
                            .collect();
                        if !pairs.is_empty() {
                            println!("  cache     : {}", pairs.join("  "));
                        }
                    }
                }
            }
        }

        if let Some(ref p) = result.placeholder {
            println!("  icon      : {p}");
        }

        if result.download_size > 0 {
            println!("  download  : {}", fmt_bytes(result.download_size));
        }

        if result.duration > 0.0 {
            println!("  time      : {}", fmt_secs(result.duration));
        }

        if show_trace {
            println!("  ── trace");

            if let Some(ref u) = trace.canonical_url {
                println!("    canonical_url     : {u}");
            }
            if let Some(ref h) = trace.cache_key {
                let src = trace.cache_key_source.as_deref().unwrap_or("url");
                println!("    cache_key         : {h}  (from {src})");
            }

            if trace.download_tail_bytes > 0 {
                let head = trace.download_bytes.saturating_sub(trace.download_tail_bytes);
                println!(
                    "    download_bytes    : {}  ({}  +  {} tail)",
                    fmt_bytes(trace.download_bytes),
                    fmt_bytes(head),
                    fmt_bytes(trace.download_tail_bytes),
                );
            } else {
                println!("    download_bytes    : {}", fmt_bytes(trace.download_bytes));
            }
            if trace.io_secs > 0.0 {
                println!("    io_secs           : {}", fmt_secs(trace.io_secs));
            }

            if trace.inspect_secs > 0.0 {
                println!("    inspect_secs      : {}", fmt_secs(trace.inspect_secs));
            }
            if trace.deliver_secs > 0.0 {
                println!("    deliver_secs      : {}", fmt_secs(trace.deliver_secs));
            }
            if let Some(ref r) = trace.job_renderer {
                println!("    job_renderer      : {r}");
            }
            let handler = get_str(&trace_json, "render_handler");
            println!("    render_handler    : {handler}");
            println!("    tier              : {}  v{}", trace.job_tier, trace.version);

            let cache_hit = get_str(&trace_json, "cache_hit");
            let cache_hit_display = if cache_hit.is_empty() { "—".to_string() } else { cache_hit };
            println!("    cache_hit         : {cache_hit_display}");

            if let Some(ref s) = trace.session_id {
                println!("    session_id        : {s}");
            }
            if let Some(ref c) = trace.customer_id {
                println!("    customer_id       : {c}");
            }
            if let Some(ref s) = trace.server {
                println!("    server            : {s}");
            }
            println!("    cancelled         : {}", trace.cancelled);
        }

        println!();
    }
}

// ── diag ──────────────────────────────────────────────────────────────────────

fn run_diag(json: bool) {
    use crate::{config::AppConfig, diag};

    let cfg = AppConfig::from_env();
    let report = diag::collect(&cfg);

    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        report.print_pretty();
    }

    if !report.healthy {
        std::process::exit(1);
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

pub fn fmt_bytes(n: u64) -> String {
    if n >= 1_048_576 { format!("{:.1} MB", n as f64 / 1_048_576.0) }
    else if n >= 1_024 { format!("{:.1} KB", n as f64 / 1_024.0) }
    else { format!("{n} B") }
}

pub fn fmt_secs(s: f64) -> String {
    if s <= 0.0 { return "—".into(); }
    if s >= 1.0 { format!("{s:.2} s") }
    else if s >= 0.001 { format!("{:.1} ms", s * 1_000.0) }
    else if s >= 0.000_001 { format!("{:.0} µs", s * 1_000_000.0) }
    else { format!("{:.0} ns", s * 1_000_000_000.0) }
}
