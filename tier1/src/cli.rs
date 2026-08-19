//! Native CLI / server entry point.
//!
//! Shared between the `tier1` and `tier2` binaries.  Each binary's `main.rs`
//! is a minimal stub that calls [`run`].
//!
//! ```text
//! <binary> serve                    # start the HTTP server
//! <binary> thumb <input> <output>   # thumbnail one source, write a JPEG
//! <binary> result <url>...          # thumbnail URLs, print result metadata
//! <binary> check                    # validate runtime config and dependencies
//! <binary> formats            # list all supported formats by kind
//! <binary> license            # print third-party license notices
//! <binary> version            # print build version
//! ```

use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::cook::Runtime;

//  CLI schema

#[derive(Parser)]
#[command(about = "Thumbrella - thumbnail and describe service")]
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

    /// Thumbnail a single source and write the JPEG to a file.
    ///
    /// `input` is a URL or a local filesystem path (promoted to a `file://`
    /// URL).  The rendered JPEG is written to `output`.  Exits non-zero when
    /// no thumbnail can be produced (placeholders included).
    Thumb {
        /// Source URL or local file path.
        #[arg(value_name = "INPUT")]
        input: String,

        /// Output file path for the JPEG thumbnail.
        #[arg(value_name = "OUTPUT")]
        output: String,
    },

    /// Thumbnail one or more URLs and print result metadata as JSON.
    ///
    /// All URLs are processed concurrently.  Default output is pretty-printed
    /// JSON with the base64 thumbnail abbreviated.  Pass `--raw` for compact,
    /// unabridged JSON (full base64 thumbnail).
    Result {
        /// Source URLs to thumbnail.
        #[arg(required = true)]
        urls: Vec<String>,

        /// Previously returned cache hints JSON (from `ThumbResult.cache`).
        ///
        /// When supplied, enables conditional fetch and client-side freshness
        /// checks.  Pass the value of the `cache` field from a prior result.
        #[arg(long)]
        cache: Option<String>,

        /// Emit compact, unabridged JSON (full base64 thumbnail) instead of
        /// the default pretty output.
        #[arg(long)]
        raw: bool,
    },

    /// Print server configuration and validate connected services.
    ///
    /// Reports tier status, cache config, account credentials, and concurrency
    /// limits.  Validates external dependencies (handoff servers, caches) where
    /// possible.  Output is private, not exposed on any HTTP endpoint.
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

//  Entry point

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
/// # Example tier 2 binary
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
    // Initialise the UX subsystem first, it controls all output.
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
        if let Some(ref hs) = cfg.handshake
            && crate::connect::looks_like_auth_token(hs)
        {
            ux.fatal(
                "TBR_HANDSHAKE looks like an auth token, this is almost certainly a mistake",
                "Auth tokens start with 'tbr_' and belong in the connect string or \
                     Authorization header, not in TBR_HANDSHAKE.  Set TBR_HANDSHAKE to a \
                     simple shared secret instead.",
            );
        }

        let rt = crate::startup::startup(&cfg).await;
        Some(hook(rt).await)
    } else {
        None
    };

    match cli.command {
        Command::Serve => run_server(runtime.unwrap()).await,
        Command::Thumb { input, output } => {
            run_thumb(input, output, runtime.unwrap()).await
        }
        Command::Result { urls, cache, raw } => {
            run_result(urls, cache, raw, runtime.unwrap()).await
        }
        Command::Check { json } => run_check(json, tier).await,
        Command::Formats { json } => run_formats(json),
        Command::Version => run_version(tier),
        Command::License => run_license(),
    }
}

//  serve

async fn run_server(runtime: Arc<Runtime>) {
    use crate::{config::AppConfig, routes};
    use axum::{
        Router,
        extract::DefaultBodyLimit,
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
                .route(
                    "/batch",
                    post(routes::batch)
                        .layer(DefaultBodyLimit::max(routes::BATCH_MAX_BODY_BYTES)),
                )
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
                &format!("could not bind port {} - address already in use", cfg.port),
                &format!(
                    "Set TBR_PORT to a different port, or stop any existing \
                     server and try again.  (details: {e})"
                ),
            );
        }
    };

    // Report the actual port (port 0 means the OS assigned an ephemeral port).
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);

    // Startup block - banner, hints, and connection info.
    ux.print_startup(
        actual_port,
        crate::TBR_VERSION,
        cfg.handshake.as_deref(),
        cfg.tier2.url.is_some(),
        cfg.tier3.url.is_some(),
    );

    // Run a lightweight diagnostic check and print a one-liner for each issue.
    //
    // We skip the port_available check here - the TcpListener::bind above
    // already either succeeded (we got a socket) or called fatal() and exited.
    // A second bind probe inside collect() would see the port as in-use by
    // this very server and produce a misleading false positive.
    {
        let mut report = crate::check::collect(&cfg);

        // Async cloud-token validation (same as run_check).
        // validate_handoff_target does a sync TCP connect only; the async
        // /health check verifies the token when present.
        for (url, headers, validation_field) in [
            (cfg.tier2.url.as_deref(), &cfg.tier2.headers, &mut report.tier2_validation),
            (cfg.tier3.url.as_deref(), &cfg.tier3.headers, &mut report.tier3_validation),
        ] {
            if let (Some(url), Some(auth)) = (url, headers.get("Authorization")) {
                match check_handoff_health(url, auth).await {
                    Ok(()) => *validation_field = crate::check::Validation::ok(),
                    Err(_) => {} // stays as Error from collect(); reported below
                }
            }
        }

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
        if let Some(ref fc) = report.cache_file_check
            && !fc.writable
        {
            issues.push(format!("cache file path is not writable: {}", fc.path));
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

//  thumb (CLI)

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

async fn run_result(
    urls: Vec<String>,
    cache_str: Option<String>,
    raw: bool,
    runtime: Arc<Runtime>,
) {
    use crate::source::CacheHints;
    use crate::{ThumbCook, cook::InputSpec};
    use futures::stream::{FuturesUnordered, StreamExt};

    let cache = cache_str.as_deref().and_then(CacheHints::decode);

    let mut pool = FuturesUnordered::new();
    for url_arg in urls {
        let is_local = !url_arg.contains("://") || url_arg.starts_with("file://");
        let url = promote_url(&url_arg);
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

    for result in &results {
        if raw {
            // --raw: compact, unabridged JSON (full base64 thumbnail).
            println!("{}", serde_json::to_string(result).unwrap());
        } else {
            // Default: pretty-printed JSON with the base64 thumbnail
            // abbreviated.  Serialise to Value so we can swap the thumbnail
            // for a short placeholder string (replacing the Vec<u8> would
            // re-encode as base64, so we do it post-serialise).
            let mut value = serde_json::to_value(result).unwrap();
            if let Some(media) = value.get_mut("media")
                && let Some(thumb) = media.get("thumbnail").and_then(|v| v.as_str())
                && thumb.len() > 200
            {
                media["thumbnail"] =
                    serde_json::Value::String(format!("<base64 jpeg data: {} bytes>", thumb.len()));
            }
            let pretty = serde_json::to_string_pretty(&value).unwrap();
            let ux = crate::ux::get();
            println!("{}", ux.colorize_json(&pretty));
        }
    }
}

/// Thumbnail a single source and write the JPEG to `output`.
///
/// `input` is a URL or a local filesystem path (promoted to a `file://` URL).
/// Exits non-zero when no thumbnail can be produced or the output cannot be
/// written.  Placeholder results (no real thumbnail) are treated as failures.
async fn run_thumb(input: String, output: String, runtime: Arc<Runtime>) {
    use crate::{ThumbCook, cook::InputSpec};

    let is_local = !input.contains("://") || input.starts_with("file://");
    let input_spec = InputSpec {
        url: promote_url(&input),
        cache: None,
        allow_local: is_local,
    };

    let cook = ThumbCook::from_input(input_spec, Arc::clone(&runtime));
    let (result, _trace, mut after) = cook.run().await;
    after.drain_spawn();

    let ux = crate::ux::get();

    let Some(media) = result.media.as_ref() else {
        ux.fatal(
            &format!("no result for {}", result.url),
            "the source could not be fetched or recognised",
        );
    };

    if media.thumbnail.is_empty() {
        let reason = if !media.placeholder.is_empty() {
            format!(" (placeholder: {})", media.placeholder)
        } else if let Some(msg) = result.message.as_deref().filter(|m| !m.is_empty()) {
            format!(" ({msg})")
        } else {
            String::new()
        };
        ux.fatal(
            &format!("no thumbnail for {}{}", result.url, reason),
            "the source may be unsupported, or the server could not render it",
        );
    }

    if let Err(e) = std::fs::write(&output, &media.thumbnail) {
        let msg = e.to_string();
        ux.fatal(&format!("could not write {output}"), &msg);
    }

    println!("wrote {output}  ({} bytes)", media.thumbnail.len());
}

//  check

async fn run_check(json: bool, tier: u8) {
    use crate::{check, config::AppConfig};

    let cfg = AppConfig::from_env();
    let mut report = check::collect(&cfg);
    report.build_tier = Some(tier);

    // For cloud: cache backends, perform the async health check that
    // validate_dsn skipped.  This sends a dummy /cache/lookup to verify
    // the auth token and cloud endpoint.
    if let Some(ref dsn) = cfg.cache_url
        && dsn.starts_with("cloud:")
    {
        let rest = dsn.strip_prefix("cloud:").unwrap_or("");
        let target = crate::connect::parse_connect_target(Some(rest.to_string()));
        match crate::cache::cloud::ping_cloud_backend(&target).await {
            Ok(()) => {
                report.cache_validation = check::Validation::ok();
            }
            Err(e) => {
                report.cache_validation = check::Validation::error(e);
                report.healthy = false;
            }
        }
    }

    // For tier2/tier3: when the connect target includes an Authorization
    // header (cloud token), validate it against the /health endpoint.
    // Only the cloud server includes the "token" field in /health.
    for (url, headers, validation_field) in [
        (cfg.tier2.url.as_deref(), &cfg.tier2.headers, &mut report.tier2_validation),
        (cfg.tier3.url.as_deref(), &cfg.tier3.headers, &mut report.tier3_validation),
    ] {
        if let (Some(url), Some(auth)) = (url, headers.get("Authorization")) {
            match check_handoff_health(url, auth).await {
                Ok(()) => {
                    *validation_field = check::Validation::ok();
                }
                Err(e) => {
                    *validation_field = check::Validation::error(e);
                    report.healthy = false;
                }
            }
        }
    }

    // Recalculate healthy after async checks may have corrected the
    // TCP-only validation from collect().
    if !report.healthy {
        report.healthy = !matches!(report.tier2_validation.status, check::ValidationStatus::Error)
            && !matches!(report.tier3_validation.status, check::ValidationStatus::Error)
            && !matches!(report.cache_validation.status, check::ValidationStatus::Error)
            && !matches!(report.trace_validation.status, check::ValidationStatus::Error)
            && !matches!(report.handshake_validation.status, check::ValidationStatus::Error)
            && report.port_available;
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

/// Check a cloud handoff target's `/health` endpoint for token validity.
///
/// The cloud server's `/health` response includes a `"token"` field.
/// `"token": true` means the token is valid; `"token": false` means the
/// token was rejected.  This check only applies to targets with an
/// Authorization header (cloud tokens).
async fn check_handoff_health(url: &str, auth_header: &str) -> Result<(), String> {
    let health_url = format!("{}/health", url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client
        .get(&health_url)
        .header("Authorization", auth_header)
        .send()
        .await
        .map_err(|e| format!("health check failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("health returned HTTP {}", resp.status().as_u16()));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("health response parse error: {e}"))?;

    match body.get("token").and_then(|v| v.as_bool()) {
        Some(true) => Ok(()),
        Some(false) => Err("token rejected by cloud server (token: false)".to_string()),
        None => {
            // Standalone servers don't include "token" in /health.
            // Absent field is fine - it's not a cloud server.
            Ok(())
        }
    }
}

//  formats

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

    println!("Thumbrella - Supported Formats\n");

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
        println!("  Tier 2 is NOT configured - set TBR_TIER2 to enable.");
    }
    if !tier3_available {
        println!("  Tier 3 is NOT configured - set TBR_TIER3 to enable.");
    }
}

//  version

fn run_version(tier: u8) {
    println!("thumbrella {}  (tier {tier})", crate::TBR_VERSION);
}

//  license

fn run_license() {
    print!("{}", include_str!("license.txt"));
}
