//! Server startup — one-time initialisation before the first request is served.

use std::sync::Arc;

use image;
use crate::cache::{self, CacheStore};
use crate::config::AppConfig;
use crate::cook::Runtime;
use crate::tracelog::{self, TraceStore};

/// Run all one-time startup tasks and return the shared [`Runtime`].
///
/// Call once from `main`, after the logger is initialised but before the
/// axum listener is bound.
pub async fn startup(cfg: &AppConfig) -> Arc<Runtime> {
    // ── 1. HTTP client warmup ─────────────────────────────────────────────────
    tracing::debug!("startup: initialising HTTP client");
    crate::http_buf::init_http_client();

    // ── 2. Cache backend ──────────────────────────────────────────────────────
    let cache = if let Some(ref dsn) = cfg.cache_url {
        match cache::open_from_dsn(dsn) {
            Ok(backends) => {
                let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
                tracing::info!("cache: opened {} ({})", dsn, names.join(", "));
                CacheStore::new(backends)
            }
            Err(e) => {
                tracing::warn!("cache: could not open {dsn}: {e}");
                CacheStore::none()
            }
        }
    } else {
        tracing::debug!("cache: no TBR_CACHE configured — running without cache");
        CacheStore::none()
    };

    // ── 3. Trace backend ──────────────────────────────────────────────────────
    let trace = if let Some(ref dsn) = cfg.trace_url {
        match tracelog::open_from_dsn(dsn) {
            Ok(backends) => {
                let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
                tracing::info!("trace: opened {} ({})", dsn, names.join(", "));
                TraceStore::new(backends)
            }
            Err(e) => {
                tracing::warn!("trace: could not open {dsn}: {e}");
                TraceStore::none()
            }
        }
    } else {
        tracing::debug!("trace: no TBR_TRACE configured — trace logging disabled");
        TraceStore::none()
    };

    // ── 4. Tier-2/3 reachability ──────────────────────────────────────────────
    // TODO: send a HEAD or /health request to cfg.tier2_url / cfg.tier3_url
    // and log a warning if the response is not 200.

    tracing::debug!("startup: complete");

    // ── 5. Background image ───────────────────────────────────────────────────
    static BG_PNG: &[u8] = include_bytes!("../assets/background.png");
    let background_image = image::load_from_memory(BG_PNG)
        .ok()
        .map(|img| img.into_rgb8());
    if background_image.is_some() {
        tracing::debug!("startup: background image loaded");
    } else {
        tracing::warn!("startup: failed to decode background.png — transparency will use solid colour");
    }

    // ── 6. Placeholder images ─────────────────────────────────────────────────
    static PH_GENERAL_JPG: &[u8] = include_bytes!("../assets/placeholder_general.jpg");
    static PH_ERROR_JPG:   &[u8] = include_bytes!("../assets/placeholder_error.jpg");

    let placeholder_general = PH_GENERAL_JPG.to_vec();
    let placeholder_error   = PH_ERROR_JPG.to_vec();

    Runtime::new(cache, trace, cfg.server.clone(), background_image, placeholder_general, placeholder_error)
}
