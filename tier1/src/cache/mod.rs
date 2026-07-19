//! Cache integration - `CacheBackend` trait, `CacheStore` runtime holder,
//! and DSN-based backend construction.
//!
//! ## Async contract
//!
//! `get` is async because Cloudflare Workers cache lookups are async JS calls.
//! `put` returns an owned [`DeferredFuture`] the caller schedules via
//! [`AfterResponse`] - writes never block the response path.
//!
//! ## Handoff note
//!
//! Handoff cooks receive [`CacheStore::none()`] - no reads, no writes.
//! The originating tier-1 node owns cache population for that request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::after::{AfterResponse, DeferredFuture};
use crate::result::ThumbResult;

#[cfg(feature = "native")]
pub mod sqlite;

#[cfg(feature = "native")]
pub mod memory;

#[cfg(feature = "native")]
pub mod cloud;

//  Backend trait

/// A single cache storage backend.
#[cfg(feature = "native")]
pub trait CacheBackend: Send + Sync {
    /// Human-readable name used in logs (e.g. `"sqlite"`, `"memory"`).
    fn name(&self) -> &'static str;

    /// Async lookup.  Returns the stored JSON string on hit, `None` on miss.
    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

    /// Return a `'static + Send` future that writes `value` under `key`.
    ///
    /// `cost` is a normalized render-complexity weight (0 = trivial,
    /// 100 = >= 1 s render).  Backends use it to favour retaining expensive
    /// entries under eviction pressure.
    ///
    /// `expires_at` is a Unix epoch timestamp after which the entry should
    /// be evicted.  Backends SHOULD purge entries past this time.
    ///
    /// The future is owned and can be handed to [`AfterResponse`] so the
    /// write runs after the HTTP response.  Errors should be swallowed inside.
    fn put(&self, key: String, value: String, cost: u8, expires_at: u64) -> DeferredFuture;
}

/// A single cache storage backend for single-threaded wasm targets.
#[cfg(not(feature = "native"))]
pub trait CacheBackend {
    fn name(&self) -> &'static str;
    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<String>> + 'a>>;
    fn put(&self, key: String, value: String, cost: u8, expires_at: u64) -> DeferredFuture;
}

//  Cache frontend (sticky + inflight)

/// Short-term sticky cache + request-coalescing frontend.
///
/// Lives in front of the durable backend and provides two services:
///
/// 1. **Sticky cache** - holds ALL successful results for a short time
///    (5 s by default) regardless of upstream `Cache-Control`.  Prevents
///    duplicate upstream fetches for near-simultaneous identical requests.
///
/// 2. **Inflight coalescing** - when a cache miss occurs, the first request
///    (the "leader") registers an in-flight slot.  Subsequent requests for
///    the same key ("joiners") wait on a oneshot channel until the leader
///    completes and fans out the result via [`CacheStore::store`].
#[cfg(feature = "native")]
mod frontend {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::channel::oneshot;
    use parking_lot::Mutex;

    use crate::result::ThumbResult;

    struct InflightSlot {
        waiters: Vec<oneshot::Sender<Arc<ThumbResult>>>,
    }

    pub(super) struct CacheFrontend {
        sticky: moka::sync::Cache<String, Arc<ThumbResult>>,
        inflight: Arc<Mutex<HashMap<String, InflightSlot>>>,
    }

    impl CacheFrontend {
        pub fn new(sticky_ttl_secs: u64) -> Self {
            Self {
                sticky: moka::sync::Cache::builder()
                    .time_to_live(Duration::from_secs(sticky_ttl_secs))
                    .max_capacity(10_000)
                    .build(),
                inflight: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        pub fn sticky_check(&self, key: &str) -> Option<ThumbResult> {
            self.sticky.get(key).map(|arc| (*arc).clone())
        }

        pub fn sticky_store(&self, key: &str, result: &ThumbResult) {
            self.sticky.insert(key.to_string(), Arc::new(result.clone()));
        }

        pub fn try_lead(&self, key: &str) -> Option<oneshot::Receiver<Arc<ThumbResult>>> {
            let mut map = self.inflight.lock();
            if map.contains_key(key) {
                let (tx, rx) = oneshot::channel();
                map.get_mut(key).unwrap().waiters.push(tx);
                Some(rx)
            } else {
                map.insert(key.to_string(), InflightSlot { waiters: vec![] });
                None
            }
        }

        pub fn complete(&self, key: &str, result: Arc<ThumbResult>) {
            let slot = self.inflight.lock().remove(key);
            if let Some(slot) = slot {
                for tx in slot.waiters {
                    let _ = tx.send(Arc::clone(&result));
                }
            }
        }

        pub fn cancel(&self, key: &str) {
            self.inflight.lock().remove(key);
        }
    }
}

//  CacheStore

/// Holds a single durable cache backend with an optional sticky+coalescing
/// frontend.
///
/// Cheap to clone - backend and frontend are behind `Arc`.
/// An empty store (`CacheStore::none()`) is used for handoff cooks and when
/// no cache is configured.
#[derive(Clone, Default)]
pub struct CacheStore {
    backend: Option<Arc<dyn CacheBackend>>,
    #[cfg(feature = "native")]
    frontend: Option<Arc<frontend::CacheFrontend>>,
}

impl CacheStore {
    /// Construct a store with a durable backend and sticky frontend.
    #[cfg(feature = "native")]
    pub fn new(backend: Arc<dyn CacheBackend>, sticky_ttl_secs: u64) -> Self {
        Self {
            backend: Some(backend),
            frontend: Some(Arc::new(frontend::CacheFrontend::new(sticky_ttl_secs))),
        }
    }

    /// Backend-only store (no sticky frontend).  Used on WASM.
    pub fn backend_only(backend: Arc<dyn CacheBackend>) -> Self {
        Self {
            backend: Some(backend),
            #[cfg(feature = "native")]
            frontend: None,
        }
    }

    /// Empty store - no reads, no writes.
    pub fn none() -> Self {
        Self::default()
    }

    /// Check the cache for `key` - frontend first, then durable backend.
    pub async fn check(&self, key: &str) -> Option<(ThumbResult, &'static str)> {
        //  1. Sticky cache (native only)
        #[cfg(feature = "native")]
        if let Some(ref fe) = self.frontend
            && let Some(result) = fe.sticky_check(key)
        {
            return Some((result, "sticky"));
        }

        //  2. Inflight coalescing (native only)
        #[cfg(feature = "native")]
        let mut is_leader = false;
        #[cfg(feature = "native")]
        if let Some(ref fe) = self.frontend {
            match fe.try_lead(key) {
                Some(rx) => {
                    // Joiner - wait for the leader with a 30 s safety timeout.
                    let result = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await;

                    match result {
                        Ok(Ok(arc)) => return Some(((*arc).clone(), "sticky")),
                        _ => {
                            // Leader failed or timed out - clean up and become
                            // the new leader.
                            fe.cancel(key);
                            is_leader = true;
                        }
                    }
                }
                None => {
                    is_leader = true; // Leader - proceed to check backend.
                }
            }
        }

        //  3. Check durable backend
        if let Some(ref backend) = self.backend
            && let Some(json) = backend.get(key).await
            && let Ok(result) = serde_json::from_str(&json)
        {
            #[cfg(feature = "native")]
            if let Some(ref fe) = self.frontend {
                fe.sticky_store(key, &result);
                fe.complete(key, Arc::new(result.clone()));
            }
            return Some((result, backend.name()));
        }

        // Miss - cancel the inflight slot so it doesn't leak.
        #[cfg(feature = "native")]
        if is_leader && let Some(ref fe) = self.frontend {
            fe.cancel(key);
        }

        None
    }

    /// Schedule a write of `result` into the durable backend via `after`.
    ///
    /// Also stores in the sticky cache and fans out to inflight joiners.
    pub fn store(
        &self,
        key: &str,
        result: &ThumbResult,
        cost: u8,
        expires_at: u64,
        after: &mut AfterResponse,
    ) {
        //  Sticky cache + inflight fan-out (always, for request dedup)
        #[cfg(feature = "native")]
        if let Some(ref fe) = self.frontend {
            fe.sticky_store(key, result);
            fe.complete(key, Arc::new(result.clone()));
        }

        //  Durable backend
        // Skip durable storage when the cache string is empty (uncacheable).
        let uncacheable = result.media.as_ref().is_none_or(|m| m.cache.is_empty());
        if uncacheable {
            return;
        }
        if let Some(ref backend) = self.backend {
            let Ok(json) = serde_json::to_string(result) else {
                return;
            };
            after.push(backend.put(key.to_string(), json, cost, expires_at));
        }
    }

    /// The name of the durable backend, for logs.
    pub fn backend_name(&self) -> &'static str {
        self.backend.as_ref().map(|b| b.name()).unwrap_or("none")
    }
}

//  Cost helper

/// Normalize total render-step duration to a cache cost (0–100).
pub fn render_cost_from_secs(render_secs: f64) -> u8 {
    let render_ms = (render_secs * 1000.0) as u64;
    (render_ms.min(1000) / 10) as u8
}

//  DSN parser

/// Build a single cache backend from a DSN string.
///
/// Supported schemes:
/// - `mem:<size>` - in-memory LRU cache (e.g. `mem:200mb`, `mem:`, default 100 MB)
/// - `sqlite:<path>[#<size>]` - persistent SQLite cache (e.g. `sqlite:cache.db#1gb`)
/// - `cloud:<token>` - cloud-service cache (e.g. `cloud:tbr_s_AbCd...`)
/// - `none:` - disable caching
#[cfg(feature = "native")]
pub fn open_from_dsn(dsn: &str) -> Result<Arc<dyn CacheBackend>, String> {
    // none: - explicit no-cache.  Must not have extra content.
    if dsn == "none:" || dsn == "none" {
        return Err("none: requested - no cache backend to open".to_string());
    }
    if dsn.starts_with("none:") {
        return Err("none: takes no parameters - use just 'none:' to disable caching".to_string());
    }

    let (scheme, rest) = dsn
        .split_once(':')
        .ok_or_else(|| format!("invalid cache spec '{dsn}' - expected <scheme>:<value>"))?;

    match scheme {
        "mem" => {
            let backend = if rest.is_empty() {
                memory::MemoryCacheBackend::default_cache()
            } else {
                let (value, kind) = memory::parse_mem_size(rest)
                    .map_err(|e| format!("mem cache: {e}"))?
                    .unwrap_or((100 * 1024 * 1024, "bytes"));
                match kind {
                    "bytes" => memory::MemoryCacheBackend::with_max_bytes(value),
                    "entries" => memory::MemoryCacheBackend::with_max_entries(value),
                    _ => unreachable!(),
                }
            };
            Ok(Arc::new(backend))
        }
        "sqlite" => {
            let (path, size_spec) = match rest.split_once('#') {
                Some((p, s)) => (p, Some(s)),
                None => (rest, None),
            };
            let max_bytes = size_spec
                .and_then(|s| memory::parse_mem_size(s).ok().flatten())
                .and_then(|(v, kind)| if kind == "bytes" { Some(v) } else { None });
            let backend = sqlite::SqliteCacheBackend::open_with_limit(path, max_bytes)
                .map_err(|e| format!("sqlite cache: {e}"))?;
            Ok(Arc::new(backend))
        }
        "cloud" => cloud::CloudCacheBackend::new(rest).map(|b| Arc::new(b) as Arc<dyn CacheBackend>),
        other => Err(format!(
            "unsupported cache scheme '{other}' - supported: mem:, sqlite:, cloud:, none:"
        )),
    }
}

/// Validate a `TBR_CACHE` DSN and produce a diagnostic report.
#[cfg(feature = "native")]
pub fn validate_dsn(dsn: &str) -> (crate::check::Validation, Option<crate::check::FileCheck>) {
    if dsn == "none:" || dsn == "none" {
        return (crate::check::Validation::ok(), None);
    }
    if dsn.starts_with("none:") {
        return (
            crate::check::Validation::error(
                "none: takes no parameters - use just 'none:' to disable caching",
            ),
            None,
        );
    }

    let scheme = dsn.split(':').next().unwrap_or(dsn);
    match scheme {
        "mem" => {
            let rest = dsn.strip_prefix("mem:").unwrap_or("");
            if rest.is_empty() {
                (crate::check::Validation::ok(), None)
            } else {
                match memory::parse_mem_size(rest) {
                    Ok(_) => (crate::check::Validation::ok(), None),
                    Err(e) => (crate::check::Validation::error(e), None),
                }
            }
        }
        "cloud" => (crate::check::Validation::skipped(), None),
        "sqlite" => {
            let rest = dsn.strip_prefix("sqlite:").unwrap_or("");
            let path = rest.split('#').next().unwrap_or(rest);
            if path.is_empty() {
                return (crate::check::Validation::error("sqlite: requires a file path"), None);
            }
            (
                crate::check::Validation::skipped(),
                Some(sqlite::SqliteCacheBackend::check(path)),
            )
        }
        _ => (
            crate::check::Validation::error(format!(
                "unknown cache scheme '{scheme}' - supported: mem:, sqlite:, cloud:, none:"
            )),
            None,
        ),
    }
}
