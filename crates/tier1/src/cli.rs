//! Native CLI / server entry point.
//!
//! Shared between the `tier1` and `tier2` binaries.  Each binary's `main.rs`
//! is a minimal stub that calls [`run`].
//!
//! ```text
//! <binary> serve              # start the HTTP server
//! <binary> thumb <url>...     # thumbnail one or more URLs
//! <binary> diag               # print config and validate services
//! <binary> batch-dir <dir>    # thumbnail every file in a directory
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
    /// TBR_PORT (8001) serve port
    /// TBR_HANDOFF code accepted for inbound /handoff requests
    /// TBR_TIER2 downstream tier2 url (optional #code fragment for outbound auth)
    /// TBR_TIER3 downstream tier3 url (optional #code fragment for outbound auth)
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

        /// Previously returned etag (applied to all URLs when supplied).
        ///
        /// Prefix encodes the header: `E…` → If-None-Match, `M…` → If-Modified-Since.
        #[arg(long)]
        etag: Option<String>,

        /// Emit machine-readable JSON instead of the default pretty text.
        #[arg(long)]
        json: bool,

        /// Show internal trace fields (download metrics, render path, IDs, …).
        #[arg(long)]
        trace: bool,
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

    /// Thumbnail every file in a directory and write an HTML report.
    ///
    /// Processes files concurrently.  Writes `report.html` in the current
    /// working directory (or the path given with --output).
    #[command(name = "batch-dir")]
    BatchDir {
        /// Directory to scan.
        dir: String,

        /// Output HTML file path (default: report.html).
        #[arg(long, default_value = "report.html")]
        output: String,
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

        /// Previously returned etag (applied to all URLs when supplied).
        #[arg(long)]
        etag: Option<String>,
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
        Command::Serve                              => run_server(runtime.unwrap()).await,
        Command::Thumb { urls, etag, json, trace } => run_thumb(urls, etag, json, trace, runtime.unwrap()).await,
        Command::Diag { json }                     => run_diag(json),
        Command::BatchDir { dir, output }          => run_batch_dir(dir, output, runtime.unwrap()).await,
        Command::StreamBatch { server, args, etag } => {
            let (server, urls) = normalize_stream_batch_args(server, args);
            run_stream_batch(server, urls, etag).await;
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
        .route("/thumb.jpeg", get(routes::thumb))
        .route("/thumb", get(routes::thumb))
        .route("/handoff", post(routes::handoff))
        .route("/batch", post(routes::batch))
        .with_state(runtime);

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));
    tracing::info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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

async fn run_thumb(urls: Vec<String>, etag: Option<String>, json: bool, show_trace: bool, runtime: Arc<Runtime>) {
    use futures::stream::{FuturesUnordered, StreamExt};
    use crate::{ThumbCook, cook::InputSpec};

    let mut pool = FuturesUnordered::new();
    for raw in urls {
        let is_local = !raw.contains("://") || raw.starts_with("file://");
        let url = promote_url(&raw);
        let input = InputSpec { url, etag: etag.clone(), allow_local: is_local };
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
            if let Some(obj) = result_val.as_object_mut() {
                if let Some(thumb) = obj.get("thumbnail") {
                    if thumb.as_str().is_some_and(|s| !s.is_empty()) {
                        obj.insert("thumbnail".into(), serde_json::Value::String("<binary image data>".into()));
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

async fn run_stream_batch(server: String, urls: Vec<String>, etag: Option<String>) {
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
                if let Some(tag) = etag.clone() {
                    crate::ThumbInput::Object(crate::ThumbObject {
                        url,
                        etag: Some(tag),
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
            let ext  = result.extension.as_deref().unwrap_or("").to_string();
            let mime = result.mime.as_deref().unwrap_or("").to_string();
            let size = result.file_size.map(fmt_bytes).unwrap_or_default();
            let parts: Vec<&str> = [kind.as_str(), ext.as_str(), mime.as_str(), size.as_str()]
                .iter().copied().filter(|s| !s.is_empty()).collect();
            if !parts.is_empty() {
                println!("  format    : {}", parts.join("  "));
            }
        }

        if let Some(obj) = result.properties.as_object() {
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

        if !result.thumbnail.is_empty() {
            println!("  thumbnail : <binary image data>  (250×200  {})",
                fmt_bytes(result.thumbnail.len() as u64));
        }

        let strategy = get_str(&result_json, "strategy");
        if !strategy.is_empty() {
            println!("  strategy  : {strategy}");
        }

        if let Some(ref e) = result.etag {
            println!("  etag      : {e}");
        }

        if let Some(ref c) = result.cache {
            if c != "miss" {
                println!("  cache     : {c}");
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

// ── batch-dir ─────────────────────────────────────────────────────────────────

async fn run_batch_dir(dir: String, output: String, runtime: Arc<Runtime>) {
    use base64::Engine as _;
    use std::fs;
    use crate::{ThumbCook, cook::InputSpec};

    let entries: Vec<std::path::PathBuf> = {
        let mut v: Vec<_> = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect(),
            Err(e) => {
                eprintln!("error: cannot read directory '{}': {e}", dir);
                std::process::exit(1);
            }
        };
        v.sort();
        v
    };

    if entries.is_empty() {
        eprintln!("warning: no files found in '{dir}'");
    }

    let mut results: Vec<(usize, crate::ThumbResult, crate::ThumbTrace)> =
        Vec::with_capacity(entries.len());

    for (i, path) in entries.iter().enumerate() {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let url = format!("file://{}", abs.display());
        let input = InputSpec { url, etag: None, allow_local: true };

        let (result, trace, mut after) =
            ThumbCook::from_input(input, Arc::clone(&runtime)).run().await;
        after.drain_spawn();

        let done = results.len() + 1;
        let total = entries.len();
        let filename = entries[i]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        eprintln!("[{done}/{total}] {filename}");
        results.push((i, result, trace));
    }

    let mut html = String::with_capacity(256 * 1024);
    html.push_str(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Thumbrella batch report</title>
<style>
  body { font-family: sans-serif; background: #f5f5f5; color: #222; margin: 0; padding: 16px; }
  h1   { font-size: 1.1rem; color: #555; margin-bottom: 16px; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); gap: 16px; }
  .card { background: #fff; border: 1px solid #ddd; border-radius: 6px; overflow: hidden; box-shadow: 0 1px 3px rgba(0,0,0,.08); }
  .thumb-wrap { background: #e8e8e8; display: flex; align-items: center; justify-content: center; height: 200px; }
  .thumb-wrap img { max-width: 100%; max-height: 200px; object-fit: contain; }
  .thumb-missing { color: #999; font-size: 0.8rem; }
  .info  { padding: 10px 12px; }
  .filename { font-size: 0.78rem; color: #555; word-break: break-all; margin-bottom: 6px; }
  details { margin-top: 6px; }
  summary { font-size: 0.72rem; color: #777; cursor: pointer; user-select: none; }
  pre { font-size: 0.68rem; background: #f0f0f0; border: 1px solid #ddd; border-radius: 4px; padding: 8px;
        overflow-x: auto; white-space: pre-wrap; word-break: break-all; color: #333;
        margin: 6px 0 0; max-height: 260px; overflow-y: auto; }
  .status-success { color: #2a7d2a; font-weight: bold; }
  .status-cached  { color: #2a7d2a; }
  .status-failed  { color: #c0392b; font-weight: bold; }
  .status-other   { color: #b07a00; }
  .thumb-size     { color: #888; font-size: 0.72rem; font-weight: normal; }
</style>
</head>
<body>
"#);

    html.push_str(&format!(
        "<h1>Thumbrella batch &mdash; <code>{}</code> &mdash; {} file(s)</h1>\n<div class=\"grid\">\n",
        html_escape(&dir),
        results.len()
    ));

    for (i, result, trace) in &results {
        let path = &entries[*i];
        let filename = path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        let thumb_b64 = if result.thumbnail.is_empty() {
            None
        } else {
            Some(base64::engine::general_purpose::STANDARD.encode(&result.thumbnail))
        };

        let mut result_val = serde_json::to_value(result).unwrap_or_default();
        if let Some(obj) = result_val.as_object_mut() {
            obj.remove("thumbnail");
        }
        let trace_val = serde_json::to_value(trace).unwrap_or_default();

        let status_str = result_val
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("failed");
        let status_class = match status_str {
            "success" => "status-success",
            "cached"  => "status-cached",
            "failed"  => "status-failed",
            _         => "status-other",
        };

        html.push_str("  <div class=\"card\">\n");

        html.push_str("    <div class=\"thumb-wrap\">");
        if let Some(b64) = &thumb_b64 {
            html.push_str(&format!(
                "<img src=\"data:image/jpeg;base64,{}\" alt=\"{}\">",
                b64,
                html_escape(&filename)
            ));
        } else {
            html.push_str("<span class=\"thumb-missing\">no thumbnail</span>");
        }
        html.push_str("</div>\n");

        html.push_str("    <div class=\"info\">\n");
        {
            let size_str = result.file_size
                .map(|n| format!(" &nbsp;<span class=\"thumb-size\">{}</span>", fmt_bytes(n)))
                .unwrap_or_default();
            let res_str = {
                let w = result.properties.get("width").and_then(|v| v.as_u64());
                let h = result.properties.get("height").and_then(|v| v.as_u64());
                match (w, h) {
                    (Some(w), Some(h)) => format!(
                        " &nbsp;<span class=\"thumb-size\">{w}\u{d7}{h}</span>"
                    ),
                    _ => String::new(),
                }
            };
            html.push_str(&format!(
                "      <div class=\"filename\">{}{}{}</div>\n",
                html_escape(&filename),
                size_str,
                res_str,
            ));
        }
        // ── caption line: status • thumb-size • download • cpu ────────────
        html.push_str(&format!(
            "      <span class=\"{}\">&#9679; {}</span>",
            status_class,
            html_escape(status_str)
        ));
        if let Some(b64) = &thumb_b64 {
            let jpeg_bytes = (b64.len() * 3 / 4) as u64;
            html.push_str(&format!(
                " &nbsp;<span class=\"thumb-size\">{}</span>",
                fmt_bytes(jpeg_bytes)
            ));
        }
        if result.download_size > 0 {
            html.push_str(&format!(
                " &nbsp;<span class=\"thumb-size\">&#8595;{}</span>",
                fmt_bytes(result.download_size)
            ));
        }
        {
            // CPU time = total - connect - io  (all in the trace)
            let cpu = (result.duration - trace.io_secs).max(0.0);
            if cpu > 0.0 {
                html.push_str(&format!(
                    " &nbsp;<span class=\"thumb-size\">&#128336;{}</span>",
                    fmt_secs(cpu)
                ));
            }
        }
        html.push('\n');

        let result_pretty = serde_json::to_string_pretty(&result_val).unwrap_or_default();
        html.push_str("      <details><summary>result</summary><pre>");
        html.push_str(&html_escape(&result_pretty));
        html.push_str("</pre></details>\n");

        let trace_pretty = serde_json::to_string_pretty(&trace_val).unwrap_or_default();
        html.push_str("      <details><summary>trace</summary><pre>");
        html.push_str(&html_escape(&trace_pretty));
        html.push_str("</pre></details>\n");

        html.push_str("    </div>\n  </div>\n");
    }

    html.push_str("</div>\n</body>\n</html>\n");

    match fs::write(&output, &html) {
        Ok(()) => println!("wrote {output}  ({} items)", results.len()),
        Err(e) => {
            eprintln!("error: cannot write '{output}': {e}");
            std::process::exit(1);
        }
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
     .replace('<', "&lt;")
     .replace('>', "&gt;")
     .replace('"', "&quot;")
}
