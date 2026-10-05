//! Cache integration - `CacheBackend` trait, `CacheStore` runtime holder,
//! and spec-based backend construction (single backends or a `+` chain).
//!
//! ## Async contract
//!
//! `get` is async because Cloudflare Workers cache lookups are async JS calls.
//! `put` returns an owned [`DeferredFuture`] the caller schedules via
//! [`AfterResponse`] - writes never block the response path.
//! Native chains also schedule conditional backfill through this scheduler as
//! soon as a read hits. Backfill runs in background tasks, without delaying the
//! hit or extending the stored deadlines.
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
use crate::result::ThumbMedia;
use crate::source::CacheHints;

/// One payload shared by ordinary cache and pin lookups.
///
/// Deadlines and access times are Unix epoch seconds. Zero disables a lookup
/// lifetime. The source URL is supplied by the caller, never stored here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub media: ThumbMedia,
    /// Freshness deadline, independent of retention. Zero requires revalidation.
    pub fresh_until: u64,
    pub cache_until: u64,
    pub pin_until: u64,
    pub last_accessed_at: u64,
    pub render_cost: u8,
}

/// Metadata exchanged when copying an entry without renewing its lifetimes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntryMetadata {
    pub fresh_until: u64,
    pub cache_until: u64,
    pub pin_until: u64,
    pub last_accessed_at: u64,
    pub render_cost: u8,
}

impl CacheEntryMetadata {
    pub fn with_media(self, mut media: ThumbMedia) -> CacheEntry {
        media.url.clear();
        CacheEntry {
            media,
            fresh_until: self.fresh_until,
            cache_until: self.cache_until,
            pin_until: self.pin_until,
            last_accessed_at: self.last_accessed_at,
            render_cost: self.render_cost,
        }
    }
}

impl CacheEntry {
    pub fn new(mut media: ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64, now: u64) -> Self {
        media.url.clear();
        Self {
            fresh_until: Self::freshness_deadline(&media.cache),
            media,
            cache_until,
            pin_until: if pin_ttl == 0 { 0 } else { now.saturating_add(pin_ttl) },
            last_accessed_at: now,
            render_cost: cost,
        }
    }

    /// Extract effective freshness once when creating or updating an entry.
    /// Missing, invalid, and non-reusable cache hints are immediately stale.
    pub fn freshness_deadline(cache: &str) -> u64 {
        CacheHints::decode(cache)
            .filter(|hints| !hints.no_cache && !hints.disallow_caching())
            .and_then(|hints| hints.expires_at)
            .unwrap_or(0)
    }

    pub fn is_fresh(&self, now: u64) -> bool {
        self.fresh_until > now
    }

    /// Update the client token and its internal freshness deadline together.
    pub fn set_cache(&mut self, cache: String) {
        self.fresh_until = Self::freshness_deadline(&cache);
        self.media.cache = cache;
    }

    pub fn retain_until(&self) -> u64 {
        self.cache_until.max(self.pin_until)
    }

    pub fn lookup(&mut self, now: u64, pin: bool, pin_ttl: u64) -> Option<ThumbMedia> {
        self.access(now, pin, pin_ttl).then(|| self.media.clone())
    }

    pub fn access(&mut self, now: u64, pin: bool, pin_ttl: u64) -> bool {
        let deadline = if pin { self.pin_until } else { self.cache_until };
        if deadline <= now {
            return false;
        }
        self.last_accessed_at = now;
        if !pin && pin_ttl > 0 {
            self.pin_until = self.pin_until.max(now.saturating_add(pin_ttl));
        }
        true
    }

    pub fn metadata(&self) -> CacheEntryMetadata {
        CacheEntryMetadata {
            fresh_until: self.fresh_until,
            cache_until: self.cache_until,
            pin_until: self.pin_until,
            last_accessed_at: self.last_accessed_at,
            render_cost: self.render_cost,
        }
    }
}

pub fn unix_now_secs() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::SystemTime::UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs()
}

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
///
/// ## The key
///
/// `key` is what the entry is addressed by, and its meaning belongs to whichever
/// backend owns the storage.  Local backends (`mem`, `sqlite`) and the sticky
/// frontend treat it as opaque; the embedder chooses it (see
/// `ThumbCook::ctx_cache_key`), defaulting to the canonical source identity.
///
/// The cloud cache backend is the exception: it has to be handed the *source
/// identity* itself, because it addresses entries by URL and lets the cloud
/// derive its own account-scoped storage key.  That holds for every deployment
/// that can configure `cloud:` - a standalone server has no account of its own,
/// so it always keys by identity.  See [`cloud::CloudCacheBackend`].
///
/// ## Freshness vs retention
///
/// `cache_until` controls ordinary lookup eligibility; `pin_ttl` initializes or
/// extends an independent pin lifetime. Zero disables pin creation/refresh.
/// Backends store [`CacheEntry`] metadata and retain media until both lifetimes
/// expire. Neither deadline makes stale `media.cache` freshness valid: a retained
/// entry with validators still needs conditional revalidation after `fresh_until`.
#[cfg(feature = "native")]
pub trait CacheBackend: Send + Sync {
    /// Human-readable name used in logs (e.g. `"sqlite"`, `"memory"`).
    fn name(&self) -> &'static str;

    /// Async lookup.  Returns the deserialized [`ThumbMedia`] on hit, `None` on miss.
    fn get<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<ThumbMedia>> + Send + 'a>> {
        Box::pin(async move { self.get_entry(key, pin_ttl).await.map(|entry| entry.media) })
    }

    /// Read by the pin deadline, without extending it.
    fn get_pin<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<ThumbMedia>> + Send + 'a>> {
        Box::pin(async move { self.get_pin_entry(key).await.map(|entry| entry.media) })
    }

    /// Internal lookup retaining the metadata needed for promotion.
    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>>;
    fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>>;

    /// Copy absolute deadlines and cost only if the destination has no live
    /// entry. A delayed promotion must not replace a newer render or active pin.
    /// Native backends enforce this atomically; cloud storage is best-effort.
    fn promote(&self, key: String, entry: CacheEntry) -> DeferredFuture;

    /// Replace metadata only for the representation actually revalidated.
    /// Preserves live pin deadlines and does not overwrite a newer render.
    fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> DeferredFuture;

    /// Return a `'static + Send` future that writes `media` under `key`.
    ///
    /// `cost` is a normalized render-complexity weight (0 = trivial,
    /// 100 = >= 1 s render).  Backends use it to favour retaining expensive
    /// entries under eviction pressure.
    ///
    /// `cache_until` is an absolute Unix epoch second; `pin_ttl` is a duration.
    /// The pin deadline is calculated when the deferred write executes.
    ///
    /// The future is owned and can be handed to [`AfterResponse`] so the
    /// write runs after the HTTP response. Errors are logged by the backend.
    fn put(&self, key: String, media: ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture;
}

/// A single cache storage backend for single-threaded wasm targets.
///
/// See the native [`CacheBackend`] for the key and freshness-vs-retention
/// contracts.
#[cfg(not(feature = "native"))]
pub trait CacheBackend {
    fn name(&self) -> &'static str;
    fn get<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<ThumbMedia>> + 'a>> {
        Box::pin(async move { self.get_entry(key, pin_ttl).await.map(|entry| entry.media) })
    }
    fn get_pin<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<ThumbMedia>> + 'a>> {
        Box::pin(async move { self.get_pin_entry(key).await.map(|entry| entry.media) })
    }
    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + 'a>>;
    fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + 'a>>;
    fn promote(&self, key: String, entry: CacheEntry) -> DeferredFuture;
    fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> DeferredFuture;
    fn put(&self, key: String, media: ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture;
}

//  Cache frontend (sticky + inflight)

/// Short-term sticky cache + request-coalescing frontend.
///
/// Lives in front of the durable backend and provides two services:
///
/// 1. **Sticky cache** - holds all final results for a short time
///    (5 s by default) regardless of upstream `Cache-Control`.  Prevents
///    duplicate upstream fetches for near-simultaneous identical requests.
///    Reads do not extend the window. Empty NotModified results can only be
///    reused by clients holding the corresponding representation.
///
/// 2. **Inflight coalescing** - when a cache miss occurs, the first request
///    (the "leader") registers an in-flight slot.  Subsequent requests for
///    the same key ("joiners") wait on a oneshot channel until the leader
///    publishes its final response. Dropping the leader releases its slot.
#[cfg(feature = "native")]
mod frontend {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::channel::oneshot;
    use parking_lot::Mutex;

    use crate::result::ThumbResult;
    use crate::source::CacheHints;

    struct InflightSlot {
        id: Arc<()>,
        waiters: Vec<oneshot::Sender<Arc<ThumbResult>>>,
    }

    pub(super) enum Lookup {
        Hit(ThumbResult),
        Leader(super::DebounceLeader),
        Join { id: Arc<()>, receiver: oneshot::Receiver<Arc<ThumbResult>> },
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
            let _inflight = self.inflight.lock();
            self.sticky.insert(key.to_string(), Arc::new(result.clone()));
        }

        pub fn lookup(self: &Arc<Self>, key: &str, client: Option<&CacheHints>) -> Lookup {
            let mut map = self.inflight.lock();
            if let Some(result) = self.sticky_check(key)
                && super::CacheStore::debounce_matches(&result, client)
            {
                return Lookup::Hit(result);
            }
            if let Some(slot) = map.get_mut(key) {
                let (tx, rx) = oneshot::channel();
                slot.waiters.push(tx);
                Lookup::Join { id: slot.id.clone(), receiver: rx }
            } else {
                let id = Arc::new(());
                map.insert(key.to_string(), InflightSlot { id: id.clone(), waiters: vec![] });
                Lookup::Leader(super::DebounceLeader {
                    frontend: self.clone(), key: key.to_string(), id,
                })
            }
        }

        pub fn complete(&self, key: &str, id: &Arc<()>, result: Arc<ThumbResult>) {
            let mut map = self.inflight.lock();
            if !map.get(key).is_some_and(|slot| Arc::ptr_eq(&slot.id, id)) {
                return;
            }
            self.sticky.insert(key.to_string(), result.clone());
            let slot = map.remove(key);
            drop(map);
            if let Some(slot) = slot {
                for tx in slot.waiters {
                    let _ = tx.send(Arc::clone(&result));
                }
            }
        }

        pub fn cancel(&self, key: &str, id: &Arc<()>) {
            let mut map = self.inflight.lock();
            if map.get(key).is_some_and(|slot| Arc::ptr_eq(&slot.id, id)) {
                map.remove(key);
            }
        }
    }
}

#[cfg(not(feature = "native"))]
mod frontend {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use crate::result::ThumbResult;

    pub(super) struct CacheFrontend {
        sticky: RefCell<HashMap<String, (web_time::Instant, ThumbResult)>>,
        ttl: std::time::Duration,
    }

    impl CacheFrontend {
        pub fn new(ttl: u64) -> Self {
            Self { sticky: RefCell::new(HashMap::new()), ttl: std::time::Duration::from_secs(ttl) }
        }

        pub fn sticky_check(&self, key: &str) -> Option<ThumbResult> {
            let mut sticky = self.sticky.borrow_mut();
            sticky.retain(|_, (inserted, _)| inserted.elapsed() < self.ttl);
            sticky.get(key).map(|(_, result)| result.clone())
        }

        pub fn sticky_store(&self, key: &str, result: &ThumbResult) {
            let mut sticky = self.sticky.borrow_mut();
            sticky.retain(|_, (inserted, _)| inserted.elapsed() < self.ttl);
            if sticky.len() >= 10_000 && !sticky.contains_key(key)
                && let Some(oldest) = sticky.iter().min_by_key(|(_, (inserted, _))| *inserted).map(|(key, _)| key.clone())
            {
                sticky.remove(&oldest);
            }
            sticky.insert(key.to_string(), (web_time::Instant::now(), result.clone()));
        }
    }
}

/// Owns one native coalescing generation until completion or cancellation.
pub(crate) struct DebounceLeader {
    frontend: Arc<frontend::CacheFrontend>,
    key: String,
    #[cfg(feature = "native")]
    id: Arc<()>,
}

impl DebounceLeader {
    pub(crate) fn complete(self, result: &ThumbResult) {
        if result.status == ResultStatus::Intermediate {
            return;
        }
        #[cfg(feature = "native")]
        self.frontend.complete(&self.key, &self.id, Arc::new(result.clone()));
        #[cfg(not(feature = "native"))]
        self.frontend.sticky_store(&self.key, result);
    }
}

impl Drop for DebounceLeader {
    fn drop(&mut self) {
        #[cfg(feature = "native")]
        self.frontend.cancel(&self.key, &self.id);
    }
}

//  CacheStore

/// Holds a single durable cache backend with an optional sticky+coalescing
/// frontend.
///
/// Cheap to clone - backend and frontend are behind `Arc`.
/// An empty store (`CacheStore::none()`) is used for handoff cooks.
/// `debounce_only` retains request debounce when durable caching is disabled.
#[derive(Clone, Default)]
pub struct CacheStore {
    backend: Option<Arc<dyn CacheBackend>>,
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

    /// Backend store with isolate-local debounce on WASM.
    pub fn backend_only(backend: Arc<dyn CacheBackend>) -> Self {
        Self {
            backend: Some(backend),
            #[cfg(feature = "native")]
            frontend: None,
            #[cfg(not(feature = "native"))]
            frontend: Some(Arc::new(frontend::CacheFrontend::new(5))),
        }
    }

    /// Empty store - no reads, no writes.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn debounce_only(ttl: u64) -> Self {
        Self { backend: None, frontend: Some(Arc::new(frontend::CacheFrontend::new(ttl))) }
    }

    /// Debounce is a fixed window from insertion, independent of freshness.
    /// An empty NotModified response belongs only to a matching client copy.
    pub fn debounce_check(&self, key: &str, client: Option<&CacheHints>) -> Option<ThumbResult> {
        let result = self.frontend.as_ref()?.sticky_check(key)?;
        Self::debounce_matches(&result, client).then_some(result)
    }

    fn debounce_matches(result: &ThumbResult, client: Option<&CacheHints>) -> bool {
        if result.source == Some(ResultSource::NotModified) {
            let Some(stored) = result.media.as_ref().and_then(|media| CacheHints::decode(&media.cache)) else { return false };
            let Some(client) = client else { return false };
            if stored.to_conditional() != client.to_conditional()
                || (stored.to_conditional().is_none() && stored.expires_at != client.expires_at)
            {
                return false;
            }
        }
        true
    }

    /// A miss returns a native leader guard that must survive until the final
    /// response. Dropping it wakes joiners; superseded leaders cannot publish.
    pub(crate) async fn debounce_lookup(
        &self, key: &str, client: Option<&CacheHints>,
    ) -> (Option<ThumbResult>, Option<DebounceLeader>) {
        #[cfg(feature = "native")]
        {
            self.debounce_lookup_with_timeout(key, client, std::time::Duration::from_secs(30)).await
        }
        #[cfg(not(feature = "native"))]
        {
            (self.debounce_check(key, client), None)
        }
    }

    #[cfg(feature = "native")]
    async fn debounce_lookup_with_timeout(
        &self, key: &str, client: Option<&CacheHints>, timeout: std::time::Duration,
    ) -> (Option<ThumbResult>, Option<DebounceLeader>) {
        if let Some(ref fe) = self.frontend {
            loop {
                let (id, receiver) = match fe.lookup(key, client) {
                    frontend::Lookup::Hit(result) => return (Some(result), None),
                    frontend::Lookup::Leader(leader) => return (None, Some(leader)),
                    frontend::Lookup::Join { id, receiver } => (id, receiver),
                };
                match tokio::time::timeout(timeout, receiver).await {
                    Ok(Ok(result)) => {
                        if Self::debounce_matches(&result, client) {
                            return (Some((*result).clone()), None);
                        }
                    }
                    Ok(Err(_)) => {}
                    Err(error) => {
                        tracing::warn!("cache debounce: waiting for a response timed out - {error}");
                        fe.cancel(key, &id);
                    }
                }
            }
        }
        (None, None)
    }

    /// Publish a final response without an inflight leader. Native cooks use
    /// their leader guard instead, so superseded responses cannot be published.
    pub fn debounce_store(&self, key: &str, result: &ThumbResult) {
        if result.status == ResultStatus::Intermediate {
            return;
        }
        if let Some(ref fe) = self.frontend {
            fe.sticky_store(key, result);
        }
    }

    /// Check the cache for raw [`ThumbMedia`] — no ThumbResult wrapper.
    ///
    /// Bypasses the sticky frontend and reads the durable backend by an
    /// already-derived `key`.  Used by the cloud `/cache/*` endpoints, which
    /// derive the key themselves and need the stored format directly.
    pub async fn check_media(&self, key: &str, pin_ttl: u64) -> Option<(ThumbMedia, &'static str)> {
        if let Some(ref backend) = self.backend
            && let Some(media) = backend.get(key, pin_ttl).await
        {
            return Some((media, backend.name()));
        }
        None
    }

    /// Read durable retention metadata without publishing a debounce result.
    /// The cook must check freshness or revalidate before serving the media.
    pub async fn check_entry(&self, key: &str, pin_ttl: u64) -> Option<(CacheEntry, &'static str)> {
        let backend = self.backend.as_ref()?;
        backend.get_entry(key, pin_ttl).await.map(|entry| (entry, backend.name()))
    }

    pub async fn get_pin_entry(&self, key: &str) -> Option<(CacheEntry, &'static str)> {
        let backend = self.backend.as_ref()?;
        backend.get_pin_entry(key).await.map(|entry| (entry, backend.name()))
    }

    pub fn promote(&self, key: String, entry: CacheEntry, after: &mut AfterResponse) {
        if let Some(ref backend) = self.backend {
            after.push(backend.promote(key, entry));
        }
    }

    pub fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String, after: &mut AfterResponse) {
        if let Some(ref backend) = self.backend {
            after.push(backend.revalidate(key, entry, expected_cache));
        }
    }

    /// Pin reads bypass the ordinary sticky and inflight frontend.
    pub async fn get_pin(&self, key: &str) -> Option<(ThumbMedia, &'static str)> {
        if let Some(ref backend) = self.backend
            && let Some(media) = backend.get_pin(key).await
        {
            return Some((media, backend.name()));
        }
        None
    }

    /// Schedule a write of `result` into the durable backend via `after`.
    ///
    /// Does not publish to debounce; the cook publishes its final response
    /// separately after completing freshness checks and response formatting.
    ///
    /// `expires_at` controls cache lookup retention, not client freshness.
    /// Uncacheable media can still be stored for pins when `pin_ttl` is positive.
    pub fn store(
        &self,
        key: &str,
        result: &ThumbResult,
        cost: u8,
        expires_at: u64,
        pin_ttl: u64,
        after: &mut AfterResponse,
    ) {
        // Progress snapshots are response-only and must never become cache entries.
        if result.status == ResultStatus::Intermediate {
            return;
        }

        //  Durable backend
        // Skip durable storage when the cache string is empty (uncacheable).
        let uncacheable = result.media.as_ref().is_none_or(|m| m.cache.is_empty());
        if let Some(ref backend) = self.backend {
            let Some(ref media) = result.media else { return };
            let cache_until = if uncacheable { 0 } else { expires_at };
            after.push(backend.put(key.to_string(), media.clone(), cost, cache_until, pin_ttl));
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

//  Retention helper

/// Validator-bearing media earns long retention but still requires revalidation
/// after freshness expires. Without validators, retention follows freshness.
/// Missing freshness uses the separate client freshness fallback.
pub fn retention_deadline(
    now: u64,
    hints: Option<&CacheHints>,
    default_cache_ttl: u64,
    default_freshness_ttl: u64,
    max_ttl: u64,
) -> u64 {
    let Some(hints) = hints else { return 0 };
    if hints.disallow_caching() {
        return 0;
    }
    let deadline = if hints.to_conditional().is_some() {
        now.saturating_add(default_cache_ttl)
    } else {
        hints.expires_at.unwrap_or_else(|| now.saturating_add(default_freshness_ttl))
    };
    deadline.min(now.saturating_add(max_ttl))
}

//  Cache spec parser
//
//  `TBR_CACHE` selects zero or more cache backends chained with `+`. Lookups
//  follow definition order; no automatic sorting is performed. Each link's
//  parameters are positional and separated by `,`, so
//  values are typed by shape rather than named.  Supported links:
//
//  - `none`              - disable caching
//  - `mem[:size]`        - in-memory LRU (default 100 MB; 200mb|2gb|500)
//  - `sqlite:path[,size]` - persistent SQLite cache
//  - `cloud:connect`     - cloud-service cache (single opaque value)

/// Suggest `+` when a spec looks like it chained backends with `,`.
///
/// `,` separates the positional parameters *within* a link (for example
/// `sqlite:path,size`); it is not a chain separator.  Catching this common
/// mistake turns an error like "expected a number before 'gb'" into a message
/// that points at the actual fix.
#[cfg(feature = "native")]
fn comma_chain_hint(spec: &str) -> Option<String> {
    const SCHEMES: [&str; 4] = ["mem", "sqlite", "cloud", "none"];
    let looks_like_link = |part: &str| {
        let part = part.trim();
        match part.split_once(':') {
            Some((scheme, _)) => SCHEMES.contains(&scheme.trim()),
            None => SCHEMES.contains(&part),
        }
    };
    if spec.split(',').skip(1).any(looks_like_link) {
        let suggestion = spec.replace(',', "+");
        Some(format!(" - hint: chain backends with '+', not ',' (try '{suggestion}')"))
    } else {
        None
    }
}

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
        backends.push(open_link(link).map_err(|e| match comma_chain_hint(spec) {
            Some(hint) => format!("{e}{hint}"),
            None => e,
        })?);
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
            Err(message) => {
                let message = match comma_chain_hint(spec) {
                    Some(hint) => format!("{message}{hint}"),
                    None => message,
                };
                return (crate::check::Validation::error(message), None);
            }
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
        let got = tokio::runtime::Runtime::new().unwrap().block_on(chain.get("key", 0));
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
    fn comma_chain_hint_suggests_plus() {
        // `,` is a positional separator, not a chain separator.  The user meant
        // to chain a sqlite and a memory backend, so the hint should say so and
        // show the corrected spec.
        let err = open_from_dsn("sqlite:cache.db,mem:1gb").err().unwrap();
        assert!(err.contains("chain backends with '+'"), "{err}");
        assert!(err.contains("sqlite:cache.db+mem:1gb"), "{err}");

        // A genuine (invalid) sqlite size must not trigger the chain hint.
        let bad = open_from_dsn("sqlite:cache.db,20").err().unwrap();
        assert!(!bad.contains("chain backends"), "{bad}");

        // The check/validation path reports the same hint.
        let (v, _) = validate_dsn("sqlite:cache.db,mem:1gb");
        assert_eq!(v.status, ValidationStatus::Error);
        let message = v.message.unwrap_or_default();
        assert!(message.contains("chain backends with '+'"), "{message}");

        // A valid `+` chain is unaffected.
        let (ok, _) = validate_dsn("sqlite:/tmp/tbr-cache-hint-test.db+mem:1gb");
        assert_eq!(ok.status, ValidationStatus::Ok);
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

    #[test]
    fn validator_retention_is_independent_of_freshness() {
        const NOW: u64 = 1_000;
        const DEFAULT: u64 = 3_600;
        const MAX: u64 = 604_800;

        for expires_at in [None, Some(NOW), Some(NOW - 500), Some(NOW + 60)] {
            for (etag, last_modified) in [
                (Some("\"v1\"".to_string()), None),
                (None, Some("Wed, 01 Jan 2020 00:00:00 GMT".to_string())),
            ] {
                let hints = CacheHints { expires_at, etag, last_modified, ..Default::default() };
                assert_eq!(retention_deadline(NOW, Some(&hints), MAX, DEFAULT, MAX), NOW + MAX);
                assert_eq!(retention_deadline(NOW, Some(&hints), 120, DEFAULT, MAX), NOW + 120);
            }
        }
    }

    #[test]
    fn without_validators_retention_follows_freshness() {
        const NOW: u64 = 1_000;
        const DEFAULT: u64 = 3_600;
        const MAX: u64 = 604_800;

        for expiry in [NOW - 1, NOW, NOW + 60, NOW + 100_000, u64::MAX] {
            let hints = CacheHints { expires_at: Some(expiry), ..Default::default() };
            assert_eq!(
                retention_deadline(NOW, Some(&hints), MAX, DEFAULT, MAX),
                expiry.min(NOW + MAX),
            );
        }
        assert_eq!(retention_deadline(NOW, Some(&CacheHints::default()), MAX, DEFAULT, MAX), NOW + DEFAULT);
        assert_eq!(retention_deadline(NOW, None, MAX, DEFAULT, MAX), 0);
        for (no_store, private) in [(true, false), (false, true)] {
            let hints = CacheHints { no_store, private, etag: Some("\"v1\"".into()), ..Default::default() };
            assert_eq!(retention_deadline(NOW, Some(&hints), MAX, DEFAULT, MAX), 0);
        }
    }

    #[test]
    fn entry_lifetimes_are_independent_and_refresh_is_monotonic() {
        let mut entry = CacheEntry::new(ThumbMedia { url: "https://example.com/a".into(), ..Default::default() }, 7, 200, 300, 100);
        assert!(entry.media.url.is_empty());
        assert_eq!(entry.retain_until(), 400);
        assert!(entry.lookup(200, false, 999).is_none());
        assert!(entry.lookup(200, true, 999).is_some());
        assert_eq!(entry.pin_until, 400);
        assert!(entry.lookup(400, true, 0).is_none());
        assert_eq!(entry.last_accessed_at, 200);
        assert!(entry.lookup(150, false, 10).is_some());
        assert_eq!(entry.pin_until, 400);
        assert!(entry.lookup(160, false, 500).is_some());
        assert_eq!(entry.pin_until, 660);

        let mut cache_only = CacheEntry::new(ThumbMedia::default(), 0, 200, 0, 100);
        assert_eq!(cache_only.pin_until, 0);
        assert!(cache_only.lookup(110, true, 0).is_none());
        assert!(cache_only.lookup(110, false, 0).is_some());
        assert_eq!(cache_only.pin_until, 0);
    }

    fn media() -> ThumbMedia {
        ThumbMedia {
            url: "https://example.com/a.jpg".into(),
            cache: CacheHints::expiring_in(60).encode(60),
            thumbnail: vec![1, 2, 3],
            ..Default::default()
        }
    }

    #[test]
    fn cooks_have_independent_retention_policies_and_separate_client_freshness() {
        let runtime = crate::Runtime::new(
            CacheStore::none(), Default::default(), None, None,
            Default::default(), Default::default(), None, false,
            5, 60, 60, 604_800, 3_600,
        );
        let mut first = crate::ThumbCook::new("https://example.com/a.jpg", runtime.clone());
        let second = crate::ThumbCook::new("https://example.com/b.jpg", runtime.clone());
        assert_eq!(first.default_cache_ttl, 604_800);
        assert_eq!(first.default_pin_ttl, 0);
        first.default_cache_ttl = 120;
        first.default_pin_ttl = 600;
        assert_eq!(second.default_cache_ttl, 604_800);
        assert_eq!(second.default_pin_ttl, 0);
        assert_eq!(runtime.cache_default_ttl_secs, 3_600);

        first.status = crate::CookStatus::Complete;
        first.src.cache_hints = Some(CacheHints { etag: Some("\"v1\"".into()), ..Default::default() });
        let token = first.to_result().media.unwrap().cache;
        let hints = CacheHints::decode(&token).unwrap();
        assert_eq!(hints.etag.as_deref(), Some("\"v1\""));
        assert!(!hints.is_fresh());

        first.src.cache_hints = Some(CacheHints::default());
        let now = unix_now_secs();
        let token = first.to_result().media.unwrap().cache;
        let hints = CacheHints::decode(&token).unwrap();
        assert!(hints.expires_at.unwrap() >= now + 3_600);
    }

    async fn check_backend_lifetimes(backend: Arc<dyn CacheBackend>) {
        let now = unix_now_secs();
        backend.put("cache".into(), media(), 5, now + 600, 0).await;
        assert!(backend.get_pin("cache").await.is_none());
        assert!(backend.get("cache", 0).await.unwrap().url.is_empty());
        assert!(backend.get("cache", 600).await.is_some());
        assert!(backend.get_pin("cache").await.is_some());
        backend.put("cache".into(), media(), 5, now - 1, 0).await;
        assert!(backend.get("cache", 600).await.is_none());
        assert!(backend.get_pin("cache").await.is_some());

        let mut pinned = media();
        pinned.cache.clear();
        backend.put("pin".into(), pinned, 5, 0, 600).await;
        assert!(backend.get("pin", 600).await.is_none());
        assert_eq!(backend.get_pin("pin").await.unwrap().thumbnail, vec![1, 2, 3]);
        backend.put("expired".into(), media(), 5, now - 1, 0).await;
        assert!(backend.get("expired", 0).await.is_none());
        assert!(backend.get_pin("expired").await.is_none());
    }

    #[tokio::test]
    async fn memory_and_sqlite_share_payloads_and_enforce_deadlines() {
        check_backend_lifetimes(Arc::new(memory::MemoryCacheBackend::with_max_entries(100))).await;
        check_backend_lifetimes(Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap())).await;
    }

    async fn check_conditional_promotion(backend: Arc<dyn CacheBackend>) {
        let now = unix_now_secs();
        let old = CacheEntry::new(media(), 10, now + 300, 600, now);
        let delayed = backend.promote("race".into(), old.clone());
        let mut newer = media();
        newer.thumbnail = vec![9];
        backend.put("race".into(), newer.clone(), 99, now + 900, 1200).await;
        delayed.await;
        let live = backend.get_entry("race", 0).await.unwrap();
        assert_eq!(live.media.thumbnail, vec![9]);
        assert_eq!(live.render_cost, 99);
        assert_eq!(live.cache_until, now + 900);

        newer.cache.clear();
        backend.put("pin".into(), newer, 99, 0, 1200).await;
        backend.promote("pin".into(), old.clone()).await;
        assert!(backend.get("pin", 0).await.is_none());
        assert_eq!(backend.get_pin("pin").await.unwrap().thumbnail, vec![9]);

        backend.put("replace-expired".into(), media(), 1, now - 1, 0).await;
        backend.promote("replace-expired".into(), old.clone()).await;
        assert_eq!(backend.get_entry("replace-expired", 0).await.unwrap().render_cost, 10);

        let mut expired = old;
        expired.cache_until = now - 1;
        expired.pin_until = now - 1;
        backend.promote("expired-copy".into(), expired).await;
        assert!(backend.get("expired-copy", 0).await.is_none());
        assert!(backend.get_pin("expired-copy").await.is_none());
    }

    #[tokio::test]
    async fn native_promotion_preserves_newer_entries_and_does_not_revive_expired_data() {
        check_conditional_promotion(Arc::new(memory::MemoryCacheBackend::with_max_entries(100))).await;
        check_conditional_promotion(Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap())).await;
    }

    #[tokio::test]
    async fn revalidation_preserves_cost_and_pins_and_cannot_overwrite_newer_tokens() {
        for backend in [
            Arc::new(memory::MemoryCacheBackend::with_max_entries(100)) as Arc<dyn CacheBackend>,
            Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap()) as Arc<dyn CacheBackend>,
        ] {
            let now = unix_now_secs();
            let mut initial = media();
            initial.cache = CacheHints { etag: Some("\"A\"".into()), ..Default::default() }.encode(60);
            backend.put("entry".into(), initial, 73, now + 500, 900).await;
            let mut refreshed = backend.get_entry("entry", 0).await.unwrap();
            let expected = refreshed.media.cache.clone();
            refreshed.set_cache(CacheHints {
                etag: Some("\"B\"".into()), expires_at: Some(now + 100), ..Default::default()
            }.encode(60));
            refreshed.cache_until = now + 1000;
            let delayed = backend.revalidate("entry".into(), refreshed.clone(), expected.clone());
            let mut newer = media();
            newer.cache = CacheHints { etag: Some("\"C\"".into()), ..Default::default() }.encode(60);
            newer.thumbnail = vec![9];
            backend.put("entry".into(), newer.clone(), 99, now + 1200, 1500).await;
            delayed.await;
            assert_eq!(backend.get_entry("entry", 0).await.unwrap().media.cache, newer.cache);

            backend.revalidate("missing".into(), refreshed.clone(), expected.clone()).await;
            let inserted = backend.get_entry("missing", 0).await.unwrap();
            assert_eq!(inserted.fresh_until, now + 100);
            assert_eq!(inserted.render_cost, 73);
            assert_eq!(inserted.pin_until, now + 900);

            let token = inserted.media.cache.clone();
            refreshed.cache_until = 0;
            refreshed.set_cache(String::new());
            backend.revalidate("missing".into(), refreshed, token).await;
            assert!(backend.get("missing", 0).await.is_none());
            let pinned = backend.get_pin_entry("missing").await.unwrap();
            assert_eq!(pinned.render_cost, 73);
            assert_eq!(pinned.fresh_until, 0);

            backend.put("unpinned".into(), newer.clone(), 99, now + 1200, 0).await;
            let mut invalidated = backend.get_entry("unpinned", 0).await.unwrap();
            invalidated.cache_until = 0;
            invalidated.set_cache(String::new());
            backend.revalidate("unpinned".into(), invalidated, newer.cache).await;
            assert!(backend.get("unpinned", 0).await.is_none());
            assert!(backend.get_pin("unpinned").await.is_none());
        }
    }

    #[tokio::test]
    async fn chain_refreshes_pins_in_every_layer() {
        let memory = Arc::new(memory::MemoryCacheBackend::with_max_entries(100));
        let sqlite = Arc::new(sqlite::SqliteCacheBackend::open(":memory:").unwrap());
        let chain = chain::ChainCacheBackend::new(vec![memory.clone(), sqlite.clone()]);
        chain.put("cache".into(), media(), 0, unix_now_secs() + 600, 0).await;
        assert!(chain.get("cache", 600).await.is_some());
        assert!(memory.get_pin("cache").await.is_some());
        assert!(sqlite.get_pin("cache").await.is_some());
    }

    #[tokio::test]
    async fn store_keeps_uncacheable_media_only_for_pins() {
        let backend = Arc::new(memory::MemoryCacheBackend::with_max_entries(100));
        let store = CacheStore::new(backend.clone(), 5);
        let mut pinned = media();
        pinned.cache.clear();
        let result = ThumbResult { status: ResultStatus::Success, media: Some(pinned), ..Default::default() };
        let mut after = AfterResponse::new();
        store.store("off", &result, 0, 0, 0, &mut after);
        store.store("pin", &result, 0, 0, 600, &mut after);
        for task in after.drain() {
            task.await;
        }
        assert!(backend.get_pin("off").await.is_none());
        assert!(store.check_media("pin", 600).await.is_none());
        assert!(store.get_pin("pin").await.is_some());
    }

    #[tokio::test]
    async fn debounce_precedes_pin_refresh_and_pin_reads_ignore_sticky_results() {
        let backend = Arc::new(memory::MemoryCacheBackend::with_max_entries(100));
        let store = CacheStore::new(backend.clone(), 5);
        let result = ThumbResult { status: ResultStatus::Success, media: Some(media()), ..Default::default() };
        let mut after = AfterResponse::new();
        store.store("entry", &result, 0, unix_now_secs() + 600, 0, &mut after);
        for task in after.drain() {
            task.await;
        }
        assert!(store.debounce_check("entry", None).is_none());
        assert!(store.check_entry("entry", 0).await.is_some());
        assert!(store.check_media("entry", 0).await.is_some());
        assert!(store.debounce_check("entry", None).is_none());
        store.debounce_store("entry", &result);
        assert!(store.get_pin("entry").await.is_none());
        assert!(store.debounce_lookup("entry", None).await.0.is_some());
        assert!(store.debounce_lookup("entry", None).await.0.is_some());
        assert!(backend.get_pin("entry").await.is_none());
        assert!(store.check_entry("entry", 600).await.is_some());
        assert!(backend.get_pin("entry").await.is_some());
    }

    #[tokio::test]
    async fn dropping_a_leader_wakes_waiters_and_releases_the_slot() {
        let store = CacheStore::debounce_only(5);
        let (_, leader) = store.debounce_lookup("cancelled", None).await;
        let fe = store.frontend.as_ref().unwrap();
        let frontend::Lookup::Join { mut receiver, .. } = fe.lookup("cancelled", None) else {
            panic!("expected a joiner");
        };
        assert!(receiver.try_recv().unwrap().is_none());
        drop(leader);
        assert!(receiver.await.is_err());
        let (hit, replacement) = store.debounce_lookup("cancelled", None).await;
        assert!(hit.is_none());
        assert!(replacement.is_some());
    }

    #[tokio::test]
    async fn timed_out_leader_cannot_complete_or_cancel_its_replacement() {
        use std::time::Duration;
        for publish in [false, true] {
            let store = CacheStore::debounce_only(5);
            let (_, old) = store.debounce_lookup("timeout", None).await;
            let old = old.unwrap();
            let old_id = old.id.clone();
            let (hit, replacement) = store.debounce_lookup_with_timeout(
                "timeout", None, Duration::from_millis(1),
            ).await;
            assert!(hit.is_none());
            let replacement = replacement.unwrap();
            let fe = store.frontend.as_ref().unwrap();
            let frontend::Lookup::Join { mut receiver, .. } = fe.lookup("timeout", None) else {
                panic!("expected a replacement joiner");
            };
            if publish {
                old.complete(&ThumbResult { message: Some("old".into()), ..Default::default() });
            } else {
                drop(old);
            }
            // A timeout belonging to an old generation must also leave the new one alone.
            fe.cancel("timeout", &old_id);
            assert!(receiver.try_recv().unwrap().is_none());
            assert!(store.debounce_check("timeout", None).is_none());
            replacement.complete(&ThumbResult { message: Some("new".into()), ..Default::default() });
            assert_eq!(receiver.await.unwrap().message.as_deref(), Some("new"));
            assert_eq!(store.debounce_check("timeout", None).unwrap().message.as_deref(), Some("new"));
        }
    }

    #[tokio::test]
    async fn late_completion_cannot_overwrite_an_already_completed_replacement() {
        let store = CacheStore::debounce_only(5);
        let (_, old) = store.debounce_lookup("late", None).await;
        let (_, replacement) = store.debounce_lookup_with_timeout(
            "late", None, std::time::Duration::from_millis(1),
        ).await;
        replacement.unwrap().complete(&ThumbResult { message: Some("new".into()), ..Default::default() });
        old.unwrap().complete(&ThumbResult { message: Some("old".into()), ..Default::default() });
        assert_eq!(store.debounce_check("late", None).unwrap().message.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn client_only_completion_does_not_resolve_a_bare_joiner() {
        let store = CacheStore::debounce_only(5);
        let (_, leader) = store.debounce_lookup("client", None).await;
        let client = CacheHints { etag: Some("\"client\"".into()), ..Default::default() };
        let mut joiner = Box::pin(store.debounce_lookup("client", None));
        std::future::poll_fn(|cx| {
            assert!(joiner.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        leader.unwrap().complete(&ThumbResult {
            source: Some(ResultSource::NotModified),
            media: Some(ThumbMedia { cache: client.encode(60), ..Default::default() }),
            ..Default::default()
        });
        let (hit, replacement) = joiner.await;
        assert!(hit.is_none());
        assert!(replacement.is_some());
        assert!(store.debounce_check("client", Some(&client)).is_some());
    }

    #[test]
    fn freshness_is_independent_of_retention_and_survives_metadata_copies() {
        let token = CacheHints { expires_at: Some(200), ..Default::default() }.encode(60);
        let mut entry = CacheEntry::new(ThumbMedia {
            cache: token.clone(), ..Default::default()
        }, 73, 300, 400, 100);
        assert_eq!(entry.fresh_until, 200);
        assert!(entry.is_fresh(199));
        assert!(!entry.is_fresh(200));
        assert_eq!(entry.retain_until(), 500);
        entry.access(199, false, 900);
        assert_eq!(entry.fresh_until, 200);
        let copy = entry.metadata().with_media(entry.media.clone());
        assert_eq!(copy.fresh_until, 200);
        let restored: CacheEntry = serde_json::from_value(serde_json::to_value(&copy).unwrap()).unwrap();
        assert_eq!(restored.fresh_until, 200);
        assert_eq!(restored.media.cache, token);
        entry.set_cache(CacheHints { expires_at: Some(600), ..Default::default() }.encode(60));
        assert_eq!(entry.fresh_until, 600);
        assert_eq!(entry.cache_until, 300);
        entry.set_cache(String::new());
        assert_eq!(entry.fresh_until, 0);
        assert!(!entry.is_fresh(0));
    }

    #[test]
    fn freshness_deadline_respects_cache_policy_and_missing_hints() {
        for hints in [
            CacheHints { no_cache: true, expires_at: Some(200), ..Default::default() },
            CacheHints { no_store: true, expires_at: Some(200), ..Default::default() },
            CacheHints { private: true, expires_at: Some(200), ..Default::default() },
        ] {
            let entry = CacheEntry::new(ThumbMedia {
                cache: hints.encode(60), ..Default::default()
            }, 0, 300, 400, 100);
            assert_eq!(entry.fresh_until, 0);
            assert!(!entry.is_fresh(100));
        }
        for cache in ["", "invalid-token"] {
            assert_eq!(CacheEntry::freshness_deadline(cache), 0);
        }
        let token = CacheHints { etag: Some("\"A\"".into()), ..Default::default() }.encode(60);
        assert_eq!(CacheEntry::freshness_deadline(&token), 1);
    }
}
