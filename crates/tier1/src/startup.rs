//! Server startup — one-time initialisation before the first request is served.
//!
//! Call [`startup`] once from `main`, after the logger is initialised but
//! before the axum listener is bound.  Everything here is best-effort: a
//! failure logs a warning but does not abort the server.

use crate::config::AppConfig;

/// Run all one-time startup tasks.
///
/// Tasks (in order):
/// 1. **HTTP client warmup** — initialises the shared reqwest client so the
///    first inbound request doesn't pay TLS root-cert loading + pool setup.
/// 2. *(TODO)* **Image codec warmup** — force-load platform decoders (libjpeg,
///    libpng, etc.) so the first decode doesn't pay dynamic-linker cost.
/// 3. *(TODO)* **Cache backend** — open SQLite / KV connection and run
///    migrations.  Store the handle in a process-global so route handlers can
///    reach it without going through `AppConfig` every time.
/// 4. *(TODO)* **Tier-2/3 health check** — verify handoff URLs are reachable
///    and log a warning if not.
pub async fn startup(cfg: &AppConfig) {
    // ── 1. HTTP client ────────────────────────────────────────────────────────
    tracing::debug!("startup: initialising HTTP client");
    crate::http_buf::init_http_client();

    // ── 2. Image codec warmup ─────────────────────────────────────────────────
    // TODO: call a cheap decode (e.g. a 1×1 JPEG in memory) to force-load
    // libjpeg / libpng so the first real request doesn't pay the cost.

    // ── 3. Cache backend ──────────────────────────────────────────────────────
    // TODO: open SQLite connection, run migrations, store handle globally.
    // Example sketch:
    //   if let Some(ref db_path) = cfg.cache_db_path {
    //       match cache::open(db_path).await {
    //           Ok(handle) => CACHE.set(handle).ok(),
    //           Err(e) => tracing::warn!("cache unavailable: {e}"),
    //       }
    //   }

    // ── 4. Tier-2/3 reachability ──────────────────────────────────────────────
    // TODO: send a HEAD or /health request to cfg.tier2_url / cfg.tier3_url
    // and log a warning if the response is not 200.

    let _ = cfg; // suppress unused warning until fields are referenced above
    tracing::debug!("startup: complete");
}
