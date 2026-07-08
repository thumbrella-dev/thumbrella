//! Native CLI / server entry point.
//!
//! Shared between the `tier1` and `tier2` binaries.  Each binary's `main.rs`
//! is a minimal stub that calls [`run`].
//!
//! ```text
//! <binary> serve              # start the HTTP server
//! <binary> thumb <url>...     # thumbnail one or more URLs
//! <binary> check              # validate runtime config and dependencies
//! <binary> formats            # list all supported formats by kind
//! <binary> license            # print third-party license notices
//! <binary> version            # print build version
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

        /// Emit raw result JSON (unwrapped, with base64 thumbnail intact).
        /// Output is `{"result": {…}}` — one object per URL, no `items` wrapper.
        #[arg(long)]
        raw: bool,
    },

    /// Print server configuration and validate connected services.
    ///
    /// Reports tier status, cache config, account credentials, and concurrency
    /// limits.  Validates external dependencies (handoff servers, caches) where
    /// possible.  Output is private — not exposed on any HTTP endpoint.
    Check {
        /// Emit machine-readable JSON instead of the default pretty text.
        #[arg(long)]
        json: bool,
    },

    /// List all supported and known formats grouped by media kind.
    ///
    /// Shows every format extension the server can process, organised by
    /// FileKind category (image, video, audio, vector, document, geometry,
    /// archive, text, binary).  For tier 3 formats, shows which are
    /// available (subcommand found at startup) and which are disabled
    /// (subcommand missing).
    Formats {
        /// Emit machine-readable JSON instead of the default pretty text.
        #[arg(long)]
        json: bool,
    },

    /// Print the build version.
    Version,

    /// Print third-party license notices for all embedded dependencies.
    License,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Initialise logging, parse arguments, and run the selected command.
///
/// Intended to be called directly from `#[tokio::main] async fn main()`.
pub async fn run() {
    run_with_hook(1, |rt| async { rt }).await;
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
pub async fn run_with_hook<F, Fut>(tier: u8, hook: F)
where
    F: FnOnce(Arc<Runtime>) -> Fut,
    Fut: std::future::Future<Output = Arc<Runtime>>,
{
    // Initialise the UX subsystem first — it controls all output.
    let ux = crate::ux::init();

    // Only enable tracing-driven logging in full mode.
    // In standard/minimal mode, all user-facing output goes through ux.
    if ux.style.show_raw_logs() {
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
            .with(tracing_subscriber::fmt::layer())
            .init();
    }

    let cli = Cli::parse();

    let runtime = if !matches!(cli.command, Command::Check { .. } | Command::Formats { .. }) {
        let cfg = crate::config::AppConfig::from_env();

        // Fail fast on handshake values that look like auth tokens.
        if let Some(ref hs) = cfg.handshake {
            if crate::connect::looks_like_auth_token(hs) {
                ux.fatal(
                    "TBR_HANDSHAKE looks like an auth token — this is almost certainly a mistake",
                    "Auth tokens start with 'tbr_' and belong in the connect string or \
                     Authorization header, not in TBR_HANDSHAKE.  Set TBR_HANDSHAKE to a \
                     simple shared secret instead.",
                );
            }
        }

        let rt = crate::startup::startup(&cfg).await;
        Some(hook(rt).await)
    } else {
        None
    };

    match cli.command {
        Command::Serve => run_server(runtime.unwrap()).await,
        Command::Thumb { urls, cache, json, raw } => {
            run_thumb(urls, cache, json, raw, runtime.unwrap()).await
        }
        Command::Check { json } => run_check(json, tier).await,
        Command::Formats { json } => run_formats(json),
        Command::Version => run_version(tier),
        Command::License => run_license(),
    }
}

// ── serve ─────────────────────────────────────────────────────────────────────

async fn run_server(runtime: Arc<Runtime>) {
    use crate::{config::AppConfig, routes};
    use axum::{
        Router,
        routing::{get, post},
    };
    use std::net::SocketAddr;

    let cfg = AppConfig::from_env();
    let ux = crate::ux::get();

    let app = Router::new()
        .route("/", get(routes::landing))
        .merge(
            Router::new()
                .route("/health", get(routes::health))
                .route("/placeholder/{kind}", get(routes::placeholder))
                .route("/thumb.jpeg", get(routes::thumb))
                .route("/thumb", get(routes::thumb))
                .route("/handoff", post(routes::handoff))
                .route("/batch", post(routes::batch))
                .fallback(routes::not_found)
                .layer(axum::middleware::from_fn_with_state(runtime.clone(), routes::require_handshake)),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(runtime);

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            ux.fatal(
                &format!("could not bind port {} — address already in use", cfg.port),
                &format!(
                    "Set TBR_PORT to a different port, or stop any existing \
                     server and try again.  (details: {e})"
                ),
            );
        }
    };

    // Report the actual port (port 0 means the OS assigned an ephemeral port).
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);

    // Startup block — banner, hints, and connection info.
    ux.print_startup(
        actual_port,
        crate::TBR_VERSION,
        cfg.handshake.as_deref(),
        cfg.tier2.url.is_some(),
        cfg.tier3.url.is_some(),
    );

    // Run a lightweight diagnostic check and print a one-liner for each issue.
    //
    // We skip the port_available check here — the TcpListener::bind above
    // already either succeeded (we got a socket) or called fatal() and exited.
    // A second bind probe inside collect() would see the port as in-use by
    // this very server and produce a misleading false positive.
    {
        let report = crate::check::collect(&cfg);
        let mut issues: Vec<String> = Vec::new();

        if matches!(report.tier2, crate::check::TierStatus::Error) {
            issues.push("tier 2 handoff target is unreachable".into());
        }
        if matches!(report.tier3, crate::check::TierStatus::Error) {
            issues.push("tier 3 handoff target is unreachable".into());
        }
        if matches!(report.tier2_validation.status, crate::check::ValidationStatus::Error) {
            issues.push("tier 2 validation failed".into());
        }
        if matches!(report.tier3_validation.status, crate::check::ValidationStatus::Error) {
            issues.push("tier 3 validation failed".into());
        }
        if matches!(report.cache_validation.status, crate::check::ValidationStatus::Error) {
            issues.push("cache backend is unreachable or misconfigured".into());
        }
        if matches!(report.trace_validation.status, crate::check::ValidationStatus::Error) {
            issues.push("trace log sink is misconfigured".into());
        }
        if matches!(report.handshake_validation.status, crate::check::ValidationStatus::Error) {
            issues.push("handshake value looks like an auth token".into());
        }
        if let Some(ref fc) = report.cache_file_check {
            if !fc.writable {
                issues.push(format!("cache file path is not writable: {}", fc.path));
            }
        }

        for issue in &issues {
            ux.print_startup_issue(issue);
        }
        if !issues.is_empty() {
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\n");
        }
    }

    if ux.style.show_raw_logs() {
        tracing::info!(%addr, "listening");
    }

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
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
    let ux = crate::ux::get();
    let show = ux.style.show_raw_logs();

    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
        if show {
            tracing::info!("received SIGINT, shutting down");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
        if show {
            tracing::info!("received SIGTERM, shutting down");
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    if show {
        tracing::info!("shutdown signal received, draining connections");
    }
}

// ── thumb (CLI) ───────────────────────────────────────────────────────────────

/// Promote a bare filesystem path to a `file://` URL.
///
/// Paths that already have a scheme (`http://`, `https://`, `file://`) are
/// returned unchanged.  Relative paths are resolved against the current
/// working directory.
pub fn promote_url(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") || raw.starts_with("file://") {
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

async fn run_thumb(
    urls: Vec<String>,
    cache_str: Option<String>,
    json: bool,
    raw: bool,
    runtime: Arc<Runtime>,
) {
    use crate::source::CacheHints;
    use crate::{ThumbCook, cook::InputSpec};
    use futures::stream::{FuturesUnordered, StreamExt};

    let cache = cache_str.as_deref().and_then(CacheHints::decode);

    let mut pool = FuturesUnordered::new();
    for raw in urls {
        let is_local = !raw.contains("://") || raw.starts_with("file://");
        let url = promote_url(&raw);
        let input = InputSpec {
            url,
            cache: cache.clone(),
            allow_local: is_local,
        };
        pool.push(ThumbCook::from_input(input, Arc::clone(&runtime)).run());
    }

    let mut results: Vec<crate::ThumbResult> = Vec::with_capacity(pool.len());
    while let Some((result, _trace, mut after)) = pool.next().await {
        after.drain_spawn();
        results.push(result);
    }

    if raw || json {
        // --raw: compact JSON with full base64 thumbnail (machine-readable).
        // --json: pretty-printed, colourised, thumbnail replaced with placeholder.
        for result in &results {
            if json {
                // Serialise to Value so we can swap the base64 thumbnail
                // for a short placeholder string.  (Replacing the Vec<u8>
                // would get re-encoded as base64, so we do it post-serialise.)
                let mut value = serde_json::to_value(&result).unwrap();
                if let Some(media) = value.get_mut("media") {
                    if let Some(thumb) = media.get("thumbnail").and_then(|v| v.as_str()) {
                        if thumb.len() > 200 {
                            media["thumbnail"] = serde_json::Value::String(format!(
                                "<base64 jpeg data: {} bytes>",
                                thumb.len()
                            ));
                        }
                    }
                }
                let pretty = serde_json::to_string_pretty(&value).unwrap();
                let ux = crate::ux::get();
                println!("{}", ux.colorize_json(&pretty));
            } else {
                println!("{}", serde_json::to_string(&result).unwrap());
            }
        }
    } else {
        print_thumb_items(&results);
    }
}

// ── thumb pretty printer ──────────────────────────────────────────────────────

pub fn print_thumb_items(results: &[crate::ThumbResult]) {
    for result in results {
        // Not-modified cache hit — compact single-line display.
        if result.source == Some(crate::result::ResultSource::NotModified) {
            println!("304  -  not modified");
            continue;
        }

        let http = result.http_status.map(|s| format!("{s}")).unwrap_or_else(|| "---".into());

        let kind = result
            .media
            .as_ref()
            .map(|m| {
                serde_json::to_value(m.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let ext = result.media.as_ref().map(|m| m.extension.as_str()).unwrap_or("");
        let file_size = result.media.as_ref().map(|m| fmt_bytes(m.file_size)).unwrap_or_default();
        let thumb_size = result.media.as_ref().and_then(|m| {
            if m.thumbnail.is_empty() {
                None
            } else {
                Some(fmt_bytes(m.thumbnail.len() as u64))
            }
        });

        let type_col = if kind.is_empty() && ext.is_empty() {
            "unknown".to_string()
        } else if ext.is_empty() {
            kind
        } else {
            format!("{kind} {ext}")
        };

        let info_col = if let Some(ref media) = result.media {
            if !media.placeholder.is_empty() {
                format!("{file_size}  ->  {} placeholder", media.placeholder)
            } else if let Some(ref thumb) = thumb_size {
                format!("{file_size}  ->  {thumb}")
            } else {
                file_size
            }
        } else {
            file_size
        };

        let msg = result
            .message
            .as_deref()
            .filter(|m| !m.is_empty())
            .map(|m| format!("  -  {m}"))
            .unwrap_or_default();

        println!(
            "{http:<4}  {dur:>8}  {type_col:<16}  {info_col}{msg}",
            http = http,
            dur = fmt_secs(result.duration),
            type_col = type_col,
            info_col = info_col,
            msg = msg,
        );

        if let Some(cache) = result.media.as_ref().map(|m| &*m.cache).filter(|c| !c.is_empty()) {
            println!("cache {cache}");
        }
    }
}

// ── check ─────────────────────────────────────────────────────────────────────

async fn run_check(json: bool, tier: u8) {
    use crate::{check, config::AppConfig};

    let cfg = AppConfig::from_env();
    let mut report = check::collect(&cfg);
    report.build_tier = Some(tier);

    // For cloud: cache backends, perform the async health check that
    // validate_dsn skipped.  This sends a dummy /cache/lookup to verify
    // the auth token and cloud endpoint.
    if let Some(ref dsn) = cfg.cache_url {
        if dsn.starts_with("cloud:") {
            let token = dsn.strip_prefix("cloud:").unwrap_or("");
            match crate::cache::cloud::ping_cloud_token(token).await {
                Ok(()) => {
                    report.cache_validation = check::Validation::ok();
                }
                Err(e) => {
                    report.cache_validation = check::Validation::error(e);
                    report.healthy = false;
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        report.print_pretty();
    }

    if !report.healthy {
        std::process::exit(1);
    }
}

// ── formats ───────────────────────────────────────────────────────────────────

/// Run the `formats` CLI command: print every known format grouped by media kind.
fn run_formats(json: bool) {
    use crate::config::AppConfig;
    use crate::dispatch::format_manifest;
    use crate::media::FileKind;
    use std::collections::BTreeMap;

    let cfg = AppConfig::from_env();
    let manifest = format_manifest();
    let ux = crate::ux::get();

    // Determine tier availability for the availability column.
    let tier2_available =
        cfg.tier2.url.is_some() || crate::check::TIER2_BUILTIN.load(std::sync::atomic::Ordering::Acquire);
    let tier3_available =
        cfg.tier3.url.is_some() || crate::check::TIER3_BUILTIN.load(std::sync::atomic::Ordering::Acquire);

    // Group entries by FileKind, picking the lowest tier for each extension
    // (some extensions appear under multiple tiers).
    let mut by_kind: BTreeMap<FileKind, BTreeMap<&str, &crate::dispatch::FormatEntry>> = BTreeMap::new();
    for entry in manifest {
        let exts = by_kind.entry(entry.kind).or_default();
        // Keep the lowest-tier entry for each extension.
        exts.entry(entry.extension)
            .and_modify(|existing| {
                if entry.tier < existing.tier {
                    *existing = entry;
                }
            })
            .or_insert(entry);
    }

    if json {
        #[derive(serde::Serialize)]
        struct FormatsOutput {
            tier2_available: bool,
            tier3_available: bool,
            groups: Vec<KindGroup>,
        }
        #[derive(serde::Serialize)]
        struct KindGroup {
            kind: String,
            extensions: Vec<ExtEntry>,
        }
        #[derive(serde::Serialize)]
        struct ExtEntry {
            extension: String,
            label: String,
            tier: u8,
            renderer: String,
            shortcut: bool,
            available: bool,
        }

        let groups: Vec<KindGroup> = by_kind
            .iter()
            .map(|(kind, exts)| {
                let mut entries: Vec<ExtEntry> = exts
                    .values()
                    .map(|e| {
                        let available = match e.tier {
                            1 => true,
                            2 => tier2_available,
                            3 => tier3_available,
                            _ => false,
                        };
                        ExtEntry {
                            extension: e.extension.to_string(),
                            label: e.label.to_string(),
                            tier: e.tier,
                            renderer: e.renderer.to_string(),
                            shortcut: e.shortcut,
                            available,
                        }
                    })
                    .collect();
                entries.sort_by(|a, b| a.extension.cmp(&b.extension));
                KindGroup {
                    kind: format!("{:?}", kind).to_lowercase(),
                    extensions: entries,
                }
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&FormatsOutput {
                tier2_available,
                tier3_available,
                groups,
            })
            .unwrap()
        );
        return;
    }

    // Pretty-print.
    // Kind ordering: Image, Video, Audio, Vector, Document, Geometry,
    //                Archive, Text, Binary, Unknown
    let kind_order: &[FileKind] = &[
        FileKind::Image,
        FileKind::Video,
        FileKind::Audio,
        FileKind::Vector,
        FileKind::Document,
        FileKind::Geometry,
        FileKind::Archive,
        FileKind::Text,
        FileKind::Binary,
        FileKind::Unknown,
    ];

    println!("Thumbrella — Supported Formats\n");

    let mut total_defined: usize = 0;
    let mut total_enabled: usize = 0;

    for &kind in kind_order {
        let Some(exts) = by_kind.get(&kind) else {
            continue;
        };
        let kind_name = format!("{:?}", kind);
        let count = exts.len();
        println!(
            "  {} {}",
            ux.bold(&kind_name),
            ux.dim(&format!("({count} {})", if count == 1 { "format" } else { "formats" })),
        );

        let mut sorted: Vec<_> = exts.values().collect();
        sorted.sort_by_key(|e| e.extension);

        for e in &sorted {
            let tier_str = format!("tier {}", e.tier);
            let (tier_col, enabled) = match e.tier {
                1 => (ux.green(&tier_str), true),
                2 if tier2_available => (ux.green(&tier_str), true),
                2 => (ux.yellow(&format!("{tier_str} (unavailable)")), false),
                3 if tier3_available && crate::dispatch::tier3_can_handle(e.extension) => {
                    (ux.green(&tier_str), true)
                }
                3 => (ux.yellow(&format!("{tier_str} (unavailable)")), false),
                _ => (tier_str.to_string(), false),
            };
            if enabled {
                total_enabled += 1;
            }
            total_defined += 1;
            let shortcut_mark = if e.shortcut { ux.dim(" [shortcut]") } else { String::new() };
            println!("    {:<8}  {:<24}  {}{}", e.extension, e.label, tier_col, shortcut_mark,);
        }
        println!();
    }

    // Summary
    println!(
        "  {}  {} defined, {} enabled",
        ux.bold("Summary:"),
        total_defined,
        ux.green(&total_enabled.to_string()),
    );
    println!();

    // Legend
    println!("  Legend:");
    println!("    {}  tier available and configured", ux.green("tier N"));
    println!(
        "    {}  tier not configured (format will use placeholder)",
        ux.yellow("tier N (unavailable)")
    );
    println!(
        "    {}  shortcut: tier 1 can extract embedded thumbnail without full decode",
        ux.dim("[shortcut]")
    );
    println!();
    println!("  Tier 1 formats are always available.");
    if !tier2_available {
        println!("  Tier 2 is NOT configured — set TBR_TIER2 to enable.");
    }
    if !tier3_available {
        println!("  Tier 3 is NOT configured — set TBR_TIER3 to enable.");
    }
}

// ── version ───────────────────────────────────────────────────────────────────

fn run_version(tier: u8) {
    println!("thumbrella {}  (tier {tier})", crate::TBR_VERSION);
}

// ── license ───────────────────────────────────────────────────────────────────

fn run_license() {
    print!("{}", include_str!("license.txt"));
}

// ── helpers ───────────────────────────────────────────────────────────────────

pub fn fmt_bytes(n: u64) -> String {
    if n >= 1_048_576 {
        format!("{:.1} MB", n as f64 / 1_048_576.0)
    } else if n >= 1_024 {
        format!("{:.1} KB", n as f64 / 1_024.0)
    } else {
        format!("{n} B")
    }
}

pub fn fmt_secs(s: f64) -> String {
    if s <= 0.0 {
        return "—".into();
    }
    if s >= 1.0 {
        format!("{s:.2} s")
    } else if s >= 0.001 {
        format!("{:.1} ms", s * 1_000.0)
    } else if s >= 0.000_001 {
        format!("{:.0} µs", s * 1_000_000.0)
    } else {
        format!("{:.0} ns", s * 1_000_000_000.0)
    }
}
