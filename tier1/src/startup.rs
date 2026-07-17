//! Server startup — one-time initialisation before the first request is served.

use std::sync::Arc;

use crate::cache::{self, CacheStore};
use crate::config::AppConfig;
use crate::cook::Runtime;
use crate::tracelog::{self, TraceStore};
use image;

/// Run all one-time startup tasks and return the shared [`Runtime`].
///
/// Call once from `main`, after the logger is initialised but before the
/// axum listener is bound.
pub async fn startup(cfg: &AppConfig) -> Arc<Runtime> {
    //  1. HTTP client warmup
    tracing::debug!("startup: initialising HTTP client");
    crate::http_buf::init_http_client();

    //  2. Cache backend
    // Sticky-cache TTL (seconds).  Every successful result is held in a
    // short-term in-memory cache for this duration regardless of upstream
    // Cache-Control.  Prevents duplicate fetches for near-simultaneous
    // identical requests and enables request coalescing.
    const STICKY_TTL_SECS: u64 = 5;

    let cache = if let Some(ref dsn) = cfg.cache_url {
        if dsn == "none:" || dsn == "none" {
            CacheStore::none()
        } else {
            match cache::open_from_dsn(dsn) {
                Ok(backend) => CacheStore::new(backend, STICKY_TTL_SECS),
                Err(e) => {
                    tracing::error!("cache: could not open {dsn}: {e} — running without cache");
                    CacheStore::none()
                }
            }
        }
    } else {
        let backend = Arc::new(cache::memory::MemoryCacheBackend::default_cache());
        CacheStore::new(backend, STICKY_TTL_SECS)
    };

    //  3. Trace backend
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

    //  4. Tier-2/3 reachability
    // TODO: send a HEAD or /health request to cfg.tier2.url / cfg.tier3.url
    // and log a warning if the response is not 200.

    tracing::debug!("startup: complete");

    //  5. Background image
    let background_image = image::load_from_memory(crate::assets::BACKGROUND_PNG)
        .ok()
        .map(|img| img.into_rgb8());
    if background_image.is_some() {
        tracing::debug!("startup: background image loaded");
    } else {
        tracing::warn!("startup: failed to decode background.png — transparency will use solid colour");
    }

    Runtime::new(
        cache,
        trace,
        cfg.server.clone(),
        background_image,
        cfg.tier2.clone(),
        cfg.tier3.clone(),
        cfg.handshake.clone(),
        cfg.allow_local,
        cfg.failure_ttl as u64,
        cfg.backoff_default as u64,
        cfg.backoff_ceiling as u64,
        cfg.cache_max_ttl_secs,
        cfg.cache_default_ttl_secs,
    )
}
