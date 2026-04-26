//! Tier 1 native entry point.
//!
//! Two modes:
//!
//! ```text
//! tier1 serve              # start the HTTP server (default port from TBR_PORT)
//! tier1 thumb <url>...     # thumbnail one or more URLs, print JSON to stdout
//! ```
//!
//! Both modes drive `ThumbCook` through the same pipeline.  The server wraps
//! that in an axum handler; the CLI runs it directly and serialises results.

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
    match cli.command {
        Command::Serve => run_server().await,
        Command::Thumb { urls, etag } => run_cli(urls, etag).await,
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
        .route("/batch", post(routes::batch))
        .route("/stream", post(routes::stream));

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

async fn run_cli(urls: Vec<String>, etag: Option<String>) {
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

    let mut items: Vec<serde_json::Value> = Vec::with_capacity(pool.len());
    while let Some((result, trace)) = pool.next().await {
        items.push(serde_json::json!({
            "result": result,
            "trace": trace,
        }));
    }

    let out = serde_json::json!({ "items": items });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
