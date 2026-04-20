//! Thumbnail response cache abstraction.
//!
//! The trait is intentionally minimal so that many different backends can be
//! plugged in without changing call-sites:
//!
//! **Current backends**
//! - [`NoOpCache`] — always misses; used until a real backend is configured.
//!
//! **Planned backends** (not yet implemented)
//! - `UpstashCache`      — Upstash Redis REST API (edge / serverless)
//! - `CloudflareKvCache` — Cloudflare Workers KV (CDN-tier, public-facing)
//! - `RedisCache`        — Redis (internal paid-tier, low-latency)
//! - `PostgresCache`     — Postgres (internal, durable, billing source-of-truth)
//!
//! ## Bookkeeping
//!
//! Every hit and miss is reported to [`CacheBackend::record_access`].  Even
//! the no-op stub has the hook so future backends can implement LRU eviction,
//! per-account billing counters, and audit logs without touching call-sites.
//!
//! ## Entry format
//!
//! Cache values are opaque `Vec<u8>` at this layer (JSON-encoded `ItemResult`).
//! Serialisation/deserialisation happens in the pipeline so backends stay
//! format-agnostic.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Outcome of a successful cache lookup.
#[derive(Debug)]
pub struct CacheHit {
    /// The serialised `ItemResult` JSON bytes to return directly to the caller.
    pub data: Vec<u8>,
}

/// Whether a cache lookup found an entry or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessResult {
    Hit,
    Miss,
}

/// Bookkeeping event emitted for every cache interaction.
///
/// Passed to [`CacheBackend::record_access`] so backends can maintain LRU
/// timestamps, per-account request counters, and billing ledgers.
#[derive(Debug)]
pub struct CacheAccess {
    /// The stable cache key for this resource (canonical URL today; may be a
    /// scoped hash in the future).
    pub cache_key: String,
    /// Whether the lookup was a hit or a miss.
    pub result: AccessResult,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Pluggable cache backend.
///
/// Methods return `Pin<Box<dyn Future>>` so the trait is object-safe
/// (`dyn CacheBackend` works without any proc-macro dependency).
///
/// Errors from backend operations are always swallowed: a cache failure is
/// treated as a miss, and a failed `put` is silently ignored.
pub trait CacheBackend: Send + Sync + 'static {
    /// Look up a previously stored result by cache key.
    ///
    /// Returns `Some(CacheHit)` on a hit, `None` on a miss or any error.
    fn get<'a>(&'a self, cache_key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheHit>> + Send + 'a>>;

    /// Store a result under its cache key.  Failures are silently ignored.
    fn put<'a>(&'a self, cache_key: &'a str, data: Vec<u8>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Record a cache access for bookkeeping (LRU, billing, audit).
    ///
    /// Called after every `get` with the appropriate [`AccessResult`], and
    /// after every `put`.  Implementations should be fast; heavy work (e.g.
    /// writing to Postgres) should be fire-and-spawned.
    fn record_access<'a>(&'a self, access: CacheAccess) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// No-op stub
// ---------------------------------------------------------------------------

/// Always-miss cache backend.
///
/// Used until a real backend is configured.  All operations are instant
/// no-ops and add zero overhead to the hot path.
pub struct NoOpCache;

impl CacheBackend for NoOpCache {
    fn get<'a>(&'a self, _cache_key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheHit>> + Send + 'a>> {
        Box::pin(async { None })
    }

    fn put<'a>(&'a self, _cache_key: &'a str, _data: Vec<u8>) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn record_access<'a>(&'a self, _access: CacheAccess) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

// ---------------------------------------------------------------------------
// Global accessor
// ---------------------------------------------------------------------------

static CACHE: OnceLock<Box<dyn CacheBackend>> = OnceLock::new();

/// Install a cache backend before the first request arrives.
///
/// Panics if called more than once.
pub fn set_cache_backend(backend: impl CacheBackend) {
    CACHE
        .set(Box::new(backend))
        .ok()
        .expect("cache backend already initialised");
}

/// Return a reference to the active cache backend.
///
/// Falls back to [`NoOpCache`] if no backend has been installed.
pub fn cache() -> &'static dyn CacheBackend {
    CACHE.get_or_init(|| Box::new(NoOpCache)).as_ref()
}
