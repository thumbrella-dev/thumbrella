//! Cache integration - `CacheBackend` trait, `CacheStore` runtime holder,
//! and spec-based backend construction (single backends or a `+` chain).
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
use crate::result::{ResultSource, ResultStatus, ThumbResult};

#[cfg(feature = "native")]
pub mod sqlite;

#[cfg(feature = "native")]
pub mod memory;

#[cfg(feature = "native")]
pub mod cloud;

#[cfg(feature = "native")]
pub mod chain;

//  Backend trait

/// A single cache storage backend.
#[cfg(feature = "native")]
pub trait CacheBackend: Send + Sync {
    /// Human-readable name used in logs (e.g. `"sqlite"`, `"memory"`).
    fn name(&self) -> &'static str;

    /// Async lookup.  Returns the deserialized [`ThumbMedia`] on hit, `None` on miss.
    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<crate::result::ThumbMedia>> + Send + 'a>>;

    /// Return a `'static + Send` future that writes `media` under `key`.
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
    fn put(&self, key: String, media: crate::result::ThumbMedia, cost: u8, expires_at: u64) -> DeferredFuture;
}

/// A single cache storage backend for single-threaded wasm targets.
#[cfg(not(feature = "native"))]
pub trait CacheBackend {
    fn name(&self) -> &'static str;
    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<crate::result::ThumbMedia>> + 'a>>;
    fn put(&self, key: String, media: crate::result::ThumbMedia, cost: u8, expires_at: u64) -> DeferredFuture;
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
            && let Some(mut result) = fe.sticky_check(key)
        {
            result.source = Some(ResultSource::Cache);
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
                        Ok(Ok(arc)) => {
                            let mut result = (*arc).clone();
                            result.source = Some(ResultSource::Cache);
                            return Some((result, "sticky"));
                        }
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
            && let Some(media) = backend.get(key).await
        {
            let result = ThumbResult {
                url: media.url.clone(),
                status: ResultStatus::Success,
                source: Some(ResultSource::Cache),
                media: Some(media),
                ..Default::default()
            };
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

    /// Check the cache for raw [`ThumbMedia`] — no ThumbResult wrapper.
    ///
    /// Used by cloud cache endpoints that need the stored format directly.
    pub async fn check_media(&self, key: &str) -> Option<(crate::result::ThumbMedia, &'static str)> {
        if let Some(ref backend) = self.backend
            && let Some(media) = backend.get(key).await
        {
            return Some((media, backend.name()));
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
        // Progress snapshots are response-only and must never become cache entries.
        if result.status == ResultStatus::Intermediate {
            return;
        }

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
            let Some(ref media) = result.media else { return };
            after.push(backend.put(key.to_string(), media.clone(), cost, expires_at));
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

//  Cache spec parser
//
//  `TBR_CACHE` selects zero or more cache backends chained with `+`, fastest
//  first.  Each link's parameters are positional and separated by `,`, so
//  values are typed by shape rather than named.  Supported links:
//
//  - `none`              - disable caching
//  - `mem[:size]`        - in-memory LRU (default 100 MB; 200mb|2gb|500)
//  - `sqlite:path[,size]` - persistent SQLite cache
//  - `cloud:connect`     - cloud-service cache (single opaque value)

/// Open the backends described by a `TBR_CACHE` spec.
///
/// Returns `Ok(None)` only for the explicit disable forms `none` (or legacy
/// `none:`).  A blank spec is an error - an unset or blank `TBR_CACHE` means
/// "use the default" and is normalized away by config before this parser is
/// reached.  A single link returns that backend directly; multiple links
/// return a [`chain::ChainCacheBackend`].
#[cfg(feature = "native")]
pub fn open_from_dsn(dsn: &str) -> Result<Option<Arc<dyn CacheBackend>>, String> {
    let spec = dsn.trim();
    if spec == "none" || spec == "none:" {
        return Ok(None);
    }
    if spec.is_empty() {
        return Err(
            "empty cache spec - omit TBR_CACHE for the default, or set TBR_CACHE=none to disable"
                .to_string(),
        );
    }

    let mut backends: Vec<Arc<dyn CacheBackend>> = Vec::new();
    for link in spec.split('+') {
        let link = link.trim();
        if link.is_empty() {
            return Err(format!("invalid cache spec '{dsn}' - empty link in chain"));
        }
        backends.push(open_link(link)?);
    }

    if backends.len() == 1 {
        Ok(Some(backends.pop().unwrap()))
    } else {
        Ok(Some(Arc::new(chain::ChainCacheBackend::new(backends))))
    }
}

/// Open a single cache link (`scheme:params` segment) into a backend.
#[cfg(feature = "native")]
fn open_link(spec: &str) -> Result<Arc<dyn CacheBackend>, String> {
    let (scheme, rest) = match spec.split_once(':') {
        Some((s, r)) => (s.trim(), r),
        None => (spec.trim(), ""),
    };

    match scheme {
        "mem" => {
            if rest.contains(',') {
                return Err("mem cache takes a single optional size: 'mem[:size]'".to_string());
            }
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
            let parts: Vec<&str> = rest.split(',').collect();
            if parts.len() > 2 {
                return Err("sqlite cache takes at most two parameters: 'sqlite:path[,size]'".to_string());
            }
            let path = parts[0].trim();
            if path.is_empty() {
                return Err("sqlite cache requires a file path: 'sqlite:path[,size]'".to_string());
            }
            let max_bytes = match parts.get(1) {
                None => None,
                Some(size) => {
                    let size = size.trim();
                    if size.is_empty() {
                        return Err("sqlite cache: empty size after ',' - expected e.g. 1gb".to_string());
                    }
                    match memory::parse_mem_size(size)
                        .map_err(|e| format!("sqlite cache: {e}"))?
                    {
                        Some((bytes, "bytes")) => Some(bytes),
                        _ => {
                            return Err("sqlite cache: size must carry a byte unit (kb/mb/gb/tb)".to_string())
                        }
                    }
                }
            };
            let backend = sqlite::SqliteCacheBackend::open_with_limit(path, max_bytes)
                .map_err(|e| format!("sqlite cache: {e}"))?;
            Ok(Arc::new(backend))
        }
        "cloud" => {
            let target = crate::connect::parse_connect_target(Some(rest.to_string()));
            cloud::CloudCacheBackend::new(target).map(|b| Arc::new(b) as Arc<dyn CacheBackend>)
        }
        other => Err(format!(
            "unsupported cache scheme '{other}' - supported: mem:, sqlite:, cloud:, none"
        )),
    }
}

/// Validate a `TBR_CACHE` spec and produce a diagnostic report.
///
/// Returns `(validation, file_check)`.  The file check covers the first
/// file-backed (sqlite) link in the chain; remaining links are validated but
/// not file-checked.
#[cfg(feature = "native")]
pub fn validate_dsn(dsn: &str) -> (crate::check::Validation, Option<crate::check::FileCheck>) {
    let spec = dsn.trim();
    if spec == "none" || spec == "none:" {
        return (crate::check::Validation::ok(), None);
    }
    if spec.is_empty() {
        return (
            crate::check::Validation::error(
                "empty cache spec - omit TBR_CACHE for the default, or set TBR_CACHE=none to disable",
            ),
            None,
        );
    }

    let mut file_check: Option<crate::check::FileCheck> = None;
    for link in spec.split('+') {
        let link = link.trim();
        if link.is_empty() {
            return (
                crate::check::Validation::error(format!("invalid cache spec '{dsn}' - empty link in chain")),
                None,
            );
        }
        match validate_link(link) {
            Ok(check) => {
                if file_check.is_none() {
                    file_check = check;
                }
            }
            Err(message) => return (crate::check::Validation::error(message), None),
        }
    }
    (crate::check::Validation::ok(), file_check)
}

/// Validate a single cache link.  Returns an optional file check for
/// file-backed links.
#[cfg(feature = "native")]
fn validate_link(spec: &str) -> Result<Option<crate::check::FileCheck>, String> {
    let (scheme, rest) = match spec.split_once(':') {
        Some((s, r)) => (s.trim(), r),
        None => (spec.trim(), ""),
    };
    match scheme {
        "mem" => {
            if rest.is_empty() {
                Ok(None)
            } else if rest.contains(',') {
                Err("mem cache takes a single optional size: 'mem[:size]'".to_string())
            } else {
                memory::parse_mem_size(rest).map(|_| None)
            }
        }
        "cloud" => Ok(None),
        "sqlite" => {
            let parts: Vec<&str> = rest.split(',').collect();
            if parts.len() > 2 {
                return Err("sqlite cache takes at most two parameters: 'sqlite:path[,size]'".to_string());
            }
            let path = parts[0].trim();
            if path.is_empty() {
                return Err("sqlite cache requires a file path: 'sqlite:path[,size]'".to_string());
            }
            if let Some(size) = parts.get(1) {
                let size = size.trim();
                if size.is_empty() {
                    return Err("sqlite cache: empty size after ',' - expected e.g. 1gb".to_string());
                }
                match memory::parse_mem_size(size)? {
                    Some((_, "bytes")) => {}
                    _ => {
                        return Err("sqlite cache: size must carry a byte unit (kb/mb/gb/tb)".to_string())
                    }
                }
            }
            Ok(Some(sqlite::SqliteCacheBackend::check(path)))
        }
        other => Err(format!(
            "unsupported cache scheme '{other}' - supported: mem:, sqlite:, cloud:, none"
        )),
    }
}

/// Human-readable summary of a `TBR_CACHE` spec for `--check` reports.
/// Cloud connect tokens are masked.
#[cfg(feature = "native")]
pub fn describe_dsn(dsn: &str) -> String {
    let spec = dsn.trim();
    if spec.is_empty() {
        return "mem (default 100 MB)".to_string();
    }
    if spec == "none" || spec == "none:" {
        return "none (cache disabled)".to_string();
    }
    spec.split('+')
        .map(|link| describe_link(link.trim()))
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(feature = "native")]
fn describe_link(spec: &str) -> String {
    let (scheme, rest) = match spec.split_once(':') {
        Some((s, r)) => (s.trim(), r),
        None => (spec.trim(), ""),
    };
    match scheme {
        "mem" => {
            if rest.is_empty() {
                "mem (default 100 MB)".to_string()
            } else {
                format!("mem:{rest}")
            }
        }
        "sqlite" => {
            let mut parts = rest.split(',');
            let path = parts.next().unwrap_or("").trim();
            let size = parts.next().map(|s| s.trim()).unwrap_or("");
            if size.is_empty() {
                format!("sqlite:{path}")
            } else {
                format!("sqlite:{path} (max {size})")
            }
        }
        "cloud" => format!("cloud:{}", crate::ux::Ux::mask_connect_string(rest)),
        _ => spec.to_string(),
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::check::ValidationStatus;

    #[test]
    fn none_disables() {
        for spec in ["none", "none:"] {
            assert!(matches!(open_from_dsn(spec), Ok(None)), "{spec:?}");
        }
        // Blank is NOT "disable" - it means default.  The parser rejects it so
        // the caller never mistakes an empty TBR_CACHE for an explicit none.
        assert!(open_from_dsn("").is_err());
        assert!(open_from_dsn("   ").is_err());
    }

    #[test]
    fn single_backends() {
        assert_eq!(open_from_dsn("mem").unwrap().unwrap().name(), "memory");
        assert_eq!(open_from_dsn("mem:200mb").unwrap().unwrap().name(), "memory");
        assert_eq!(open_from_dsn("mem:500").unwrap().unwrap().name(), "memory");
    }

    #[test]
    fn chain_builds() {
        let chain = open_from_dsn("mem:20+mem:30").unwrap().unwrap();
        assert_eq!(chain.name(), "chain");
        // A chain of two memory backends reads and writes without panicking.
        let got = tokio::runtime::Runtime::new().unwrap().block_on(chain.get("key"));
        assert!(got.is_none());
    }

    #[test]
    fn reject_invalid() {
        assert!(open_from_dsn("bogus:x").is_err());
        assert!(open_from_dsn("mem:1gb,2gb").is_err()); // mem takes one param
        assert!(open_from_dsn("sqlite:").is_err()); // path required
        assert!(open_from_dsn("sqlite:/tmp/x.db,20").is_err()); // size needs a byte unit
        assert!(open_from_dsn("mem++mem").is_err()); // empty link
        assert!(open_from_dsn("none+mem").is_err()); // none cannot chain
    }

    #[test]
    fn validate_and_describe() {
        let spec = "mem:64mb+sqlite:/tmp/tbr-cache-test.db,1gb";
        let (v, fc) = validate_dsn(spec);
        assert_eq!(v.status, ValidationStatus::Ok);
        assert!(fc.is_some());
        assert_eq!(
            describe_dsn(spec),
            "mem:64mb+sqlite:/tmp/tbr-cache-test.db (max 1gb)"
        );
        let (bad, _) = validate_dsn("mem:64mb+bogus");
        assert_eq!(bad.status, ValidationStatus::Error);
        let (blank, _) = validate_dsn("");
        assert_eq!(blank.status, ValidationStatus::Error);
        assert_eq!(describe_dsn("none"), "none (cache disabled)");
    }
}
