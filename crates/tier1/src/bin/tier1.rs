//! Tier 1 native entry point.
//!
//! Subcommands:
//!
//! ```text
//! tier1 serve              # start the HTTP server
//! tier1 thumb <url>...     # thumbnail one or more URLs, print JSON to stdout
//! tier1 diag               # print server config and validate connected services
//! ```

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

// ── CLI schema ─────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "tier1", about = "Thumbrella Tier 1 — thumbnail and describe service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    ///
    /// Port and other options come from environment variables (TBR_PORT, etc.).
    Serve,

    /// Thumbnail one or more URLs and print JSON results to stdout.
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
}

// ── Entry point ────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    // Run startup for all commands that may make outbound HTTP requests.
    // Diag is the only command that doesn't need it.
    if !matches!(cli.command, Command::Diag { .. }) {
        let cfg = tier1::config::AppConfig::from_env();
        tier1::startup::startup(&cfg).await;
    }

    match cli.command {
        Command::Serve => run_server().await,
        Command::Thumb { urls, etag, json, trace } => run_cli(urls, etag, json, trace).await,
        Command::Diag { json } => run_diag(json),
    }
}

// ── serve ─────────────────────────────────────────────────────────────────────

async fn run_server() {
    use axum::{Router, routing::{get, post}};
    use std::net::SocketAddr;
    use tier1::{config::AppConfig, routes};

    let cfg = AppConfig::from_env();
    tracing::info!(version = tier1::TBR_VERSION, "tier1 starting");

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/thumb.jpeg", get(routes::thumb))
        .route("/batch", post(routes::batch));

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
fn promote_url(raw: &str) -> String {
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

async fn run_cli(urls: Vec<String>, etag: Option<String>, json: bool, show_trace: bool) {
    use futures::stream::{FuturesUnordered, StreamExt};
    use tier1::{ThumbCook, ThumbSpec};

    let mut pool = FuturesUnordered::new();
    for raw in urls {
        // allow_local = true for bare paths (promoted to file://) and for
        // explicit file:// URLs.  http/https are not local.
        let is_local = !raw.contains("://") || raw.starts_with("file://");
        let url = promote_url(&raw);
        let spec = ThumbSpec { url, etag: etag.clone(), allow_local: is_local };
        pool.push(ThumbCook::new(spec).run());
    }

    let mut items: Vec<(tier1::ThumbResult, tier1::ThumbTrace)> = Vec::with_capacity(pool.len());
    while let Some(pair) = pool.next().await {
        items.push(pair);
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

// ── thumb pretty printer ──────────────────────────────────────────────────────

fn print_thumb_items(items: &[(tier1::ThumbResult, tier1::ThumbTrace)], show_trace: bool) {
    for (result, trace) in items {
        // Serialize once — lets us pull string representations of enum fields
        // without reimplementing the serde rename logic.
        let result_json = serde_json::to_value(result).unwrap_or_default();
        let trace_json  = serde_json::to_value(trace).unwrap_or_default();
        let get_str = |obj: &serde_json::Value, key: &str| -> String {
            obj.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .replace('_', " ")
        };

        // Header: URL
        let url_display = result.url.as_str();
        let sep_len = (56usize).saturating_sub(url_display.len() + 4);
        let sep = "─".repeat(sep_len.max(4));
        println!("── {url_display} {sep}");

        // Status (+ message if non-empty)
        let status = get_str(&result_json, "status");
        let status_str = if !result.message.is_empty() {
            format!("{status}  ({})", result.message)
        } else {
            status
        };
        println!("  status    : {status_str}");

        // Format: kind  extension  mime  file_size
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

        // Properties — plain key=value pairs
        if let Some(ref props) = result.properties {
            if let Some(obj) = props.as_object() {
                let pairs: Vec<String> = obj.iter()
                    .map(|(k, v)| {
                        // Strip JSON quotes from string values
                        if let Some(s) = v.as_str() { format!("{k}={s}") }
                        else { format!("{k}={v}") }
                    })
                    .collect();
                if !pairs.is_empty() {
                    println!("  properties: {}", pairs.join("  "));
                }
            }
        }

        // Thumbnail output (size from actual bytes in result)
        if !result.thumbnail.is_empty() {
            println!("  thumbnail : <binary image data>  (250\u{d7}200  {})",
                fmt_bytes(result.thumbnail.len() as u64));
        }

        // Strategy
        let strategy = get_str(&result_json, "strategy");
        if !strategy.is_empty() {
            println!("  strategy  : {strategy}");
        }

        // Etag
        if let Some(ref e) = result.etag {
            println!("  etag      : {e}");
        }

        // Cache outcome (only if interesting)
        if let Some(ref c) = result.cache {
            if c != "miss" {
                println!("  cache     : {c}");
            }
        }

        // Placeholder token
        if let Some(ref p) = result.placeholder {
            println!("  icon      : {p}");
        }

        // Download size
        if result.download_size > 0 {
            println!("  download  : {}", fmt_bytes(result.download_size));
        }

        // Wall-clock duration
        if result.duration > 0.0 {
            println!("  time      : {}", fmt_secs(result.duration));
        }

        // ── Trace section (--trace) ───────────────────────────────────────
        if show_trace {
            println!("  ── trace");

            // URLs
            if let Some(ref u) = trace.canonical_url {
                println!("    canonical_url     : {u}");
            }
            if trace.final_url.as_deref() != trace.canonical_url.as_deref() {
                if let Some(ref u) = trace.final_url {
                    println!("    final_url         : {u}");
                }
            }
            if let Some(ref h) = trace.cache_hash {
                let src = trace.cache_hash_source.as_deref().unwrap_or("url");
                println!("    cache_hash        : {h}  (from {src})");
            }

            // Download detail
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
            if trace.connect_secs > 0.0 {
                println!("    connect_secs      : {}", fmt_secs(trace.connect_secs));
            }

            // Phase timing
            if trace.inspect_secs > 0.0 {
                println!("    inspect_secs      : {}", fmt_secs(trace.inspect_secs));
            }
            if trace.shortcut_secs > 0.0 {
                println!("    shortcut_secs     : {}", fmt_secs(trace.shortcut_secs));
            }
            if trace.render_secs > 0.0 {
                println!("    render_secs       : {}", fmt_secs(trace.render_secs));
            }
            if trace.deliver_secs > 0.0 {
                println!("    deliver_secs      : {}", fmt_secs(trace.deliver_secs));
            }
            if let Some([w, h]) = trace.render_resolution {
                println!("    render_resolution : {w}\u{d7}{h}");
            }

            // Job provenance
            if let Some(ref r) = trace.job_renderer {
                println!("    job_renderer      : {r}");
            }
            if let Some(ref c) = trace.job_codec {
                println!("    job_codec         : {c}");
            }
            if let Some(v) = trace.video_seek_secs {
                println!("    video_seek_secs   : {}", fmt_secs(v));
            }

            // Render handler (always shown)
            let handler = get_str(&trace_json, "render_handler");
            println!("    render_handler    : {handler}");

            // Tier + version
            println!("    tier              : {}  v{}", trace.job_tier, trace.version);

            // Cache
            let cache_hit = get_str(&trace_json, "cache_hit");
            let cache_hit_display = if cache_hit.is_empty() { "—".to_string() } else { cache_hit };
            println!("    cache_hit         : {cache_hit_display}");

            // Attribution
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

fn fmt_bytes(n: u64) -> String {
    if n >= 1_048_576 { format!("{:.1} MB", n as f64 / 1_048_576.0) }
    else if n >= 1_024 { format!("{:.1} KB", n as f64 / 1_024.0) }
    else { format!("{n} B") }
}

fn fmt_secs(s: f64) -> String {
    if s <= 0.0 { return "—".into(); }
    if s >= 1.0 { format!("{s:.2} s") }
    else if s >= 0.001 { format!("{:.1} ms", s * 1_000.0) }
    else if s >= 0.000_001 { format!("{:.0} µs", s * 1_000_000.0) }
    else { format!("{:.0} ns", s * 1_000_000_000.0) }
}

// ── diag ──────────────────────────────────────────────────────────────────────

fn run_diag(json: bool) {
    use tier1::{config::AppConfig, diag};

    let cfg = AppConfig::from_env();
    let report = diag::collect(&cfg);

    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        report.print_pretty();
    }

    // Exit non-zero when any component is degraded so CI / healthcheck scripts
    // can detect misconfiguration without parsing the output.
    if !report.healthy {
        std::process::exit(1);
    }
}
