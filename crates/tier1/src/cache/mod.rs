//! Cache integration — `CacheBackend` trait, `CacheStore` runtime holder,
//! process-global backend registry, and DSN-based backend construction.
//!
//! ## Async contract
//!
//! `get` is async because Cloudflare Workers cache lookups are async JS calls.
//! `put_task` returns an owned [`DeferredFuture`] the caller schedules via
//! [`AfterResponse`] — writes never block the response path.
//!
//! ## Global cache store
//!
//! Call [`init_global`] once at startup (native only).  Every subsequent
//! `ThumbCook::new()` call picks up the backends via [`global()`].
//!
//! ## Handoff note
//!
//! Handoff cooks receive [`CacheStore::none()`] — no reads, no writes.
//! The originating tier-1 node owns cache population for that request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::after::{AfterResponse, DeferredFuture};
use crate::result::ThumbResult;

#[cfg(feature = "native")]
pub mod sqlite;

// ── Backend trait ─────────────────────────────────────────────────────────────

/// A single cache storage backend.  Implementations must be `Send + Sync`.
pub trait CacheBackend: Send + Sync {
    /// Human-readable name used in logs (e.g. `"sqlite"`, `"redis"`).
    fn name(&self) -> &'static str;

    /// Async lookup.  Returns the stored JSON string on hit, `None` on miss.
    /// Also bumps `last_accessed_at` / `access_count` in-place.
    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

    /// Return a `'static + Send` future that writes `value` under `key`.
    ///
    /// The future is owned and can be handed to [`AfterResponse`] so the
    /// write runs after the HTTP response.  Errors should be swallowed inside.
    fn put_task(&self, key: String, value: String) -> DeferredFuture;
}

// ── CacheStore ────────────────────────────────────────────────────────────────

/// Holds the active cache backends for one request.
///
/// Cheap to clone — backends are behind `Arc`.  An empty store
/// (`CacheStore::none()`) is used for handoff cooks and when no cache is
/// configured.
#[derive(Clone, Default)]
pub struct CacheStore {
    backends: Vec<Arc<dyn CacheBackend>>,
}

impl CacheStore {
    /// Construct a store from a list of backends.
    pub fn new(backends: Vec<Arc<dyn CacheBackend>>) -> Self {
        Self { backends }
    }

    /// Empty store — no reads, no writes.
    pub fn none() -> Self {
        Self { backends: Vec::new() }
    }

    /// Check all backends for `key` in order.
    ///
    /// On the first hit, propagates the value back into earlier-index backends
    /// (best-effort, fire-and-forget) so hotter caches warm from cooler ones.
    pub async fn check(&self, key: &str) -> Option<ThumbResult> {
        if self.backends.is_empty() { return None; }

        let mut hit_json:  Option<String> = None;
        let mut hit_index: usize = 0;

        for (i, backend) in self.backends.iter().enumerate() {
            if let Some(v) = backend.get(key).await {
                hit_json  = Some(v);
                hit_index = i;
                break;
            }
        }

        let json = hit_json?;

        // Back-propagate into earlier (hotter) backends that missed.
        #[cfg(feature = "native")]
        for backend in &self.backends[..hit_index] {
            let b = Arc::clone(backend);
            let k = key.to_string();
            let v = json.clone();
            tokio::task::spawn(async move { b.put_task(k, v).await });
        }
        #[cfg(not(feature = "native"))]
        for backend in &self.backends[..hit_index] {
            backend.put_task(key.to_string(), json.clone()).await;
        }

        serde_json::from_str(&json).ok()
    }

    /// Schedule writes of `result` into all backends via `after`.
    ///
    /// Writes are deferred — they run after the HTTP response via
    /// [`AfterResponse::drain_spawn`] (native) or `ctx.wait_until` (Workers).
    pub fn store(&self, key: &str, result: &ThumbResult, after: &mut AfterResponse) {
        if self.backends.is_empty() { return; }
        let Ok(json) = serde_json::to_string(result) else { return };
        for backend in &self.backends {
            after.push(backend.put_task(key.to_string(), json.clone()));
        }
    }
}

// ── DSN parser ────────────────────────────────────────────────────────────────

/// Build a backend list from a DSN string.
///
/// Supported schemes:
/// - `sqlite:<path>` — SQLite file cache (`<path>` may be absolute or relative)
///
/// Returns an error string if the scheme is unknown or the backend fails to open.
#[cfg(feature = "native")]
pub fn open_from_dsn(dsn: &str) -> Result<Vec<Arc<dyn CacheBackend>>, String> {
    if let Some(path) = dsn.strip_prefix("sqlite:") {
        let backend = sqlite::SqliteCacheBackend::open(path)
            .map_err(|e| format!("sqlite cache: {e}"))?;
        return Ok(vec![Arc::new(backend)]);
    }
    Err(format!("unsupported cache DSN scheme: {dsn}"))
}

/// Validate a `TBR_CACHE` DSN and produce a diagnostic report.
///
/// Returns `(validation, file_check)` where:
/// - `validation` is `Error` for unknown schemes, `Skipped` for known ones
///   (deeper per-file checks live in `file_check`)
/// - `file_check` is `Some` for file-backed schemes, `None` otherwise
#[cfg(feature = "native")]
pub fn validate_dsn(dsn: &str) -> (crate::diag::Validation, Option<crate::diag::FileCheck>) {
    if let Some(path) = dsn.strip_prefix("sqlite:") {
        return (crate::diag::Validation::skipped(), Some(sqlite::SqliteCacheBackend::diag(path)));
    }
    let scheme = dsn.split(':').next().unwrap_or(dsn);
    (
        crate::diag::Validation::error(format!(
            "unknown cache DSN scheme '{scheme}' — supported: sqlite:<path>"
        )),
        None,
    )
}

