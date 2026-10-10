//! Layered cache chain - read through tiers in order, write through to all.
//!
//! A chain is a list of backends (L1, L2, ...) selected by a `TBR_CACHE`
//! spec such as `mem:64mb+sqlite:/var/cache.db,1gb`.  Tier order is the
//! order written, fastest first.
//!
//! ## Policy
//!
//! - `get` returns the first hit. When refreshing pins it also consults later
//!   tiers so durable copies receive the same lifetime extension.
//! - `get_pin` returns the first entry with a live pin deadline.
//! - `put` writes through to every tier, so each layer is populated together.
//! - Public pin aliases belong to the final (coldest) tier. One authoritative
//!   index prevents cross-layer collisions and survives loss of a memory front.
//!
//! Both lookup paths asynchronously backfill earlier tiers that missed, without
//! extending deadlines. Promotion uses the native [`AfterResponse`] scheduler
//! and does not wait for writes. A destination with a live entry is left alone.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::after::{AfterResponse, DeferredFuture};
use crate::cache::{CacheBackend, CacheEntry};
use crate::cache::{PinFuture, pins::PinClaim};
use crate::result::ThumbMedia;

/// A read-through, write-through chain of cache backends.
pub struct ChainCacheBackend {
    tiers: Vec<Arc<dyn CacheBackend>>,
}

impl ChainCacheBackend {
    /// Build a chain from at least two backends (index 0 = L1, fastest).
    pub fn new(tiers: Vec<Arc<dyn CacheBackend>>) -> Self {
        debug_assert!(tiers.len() >= 2);
        Self { tiers }
    }

    /// The configured tier names joined with `+`, e.g. `mem+sqlite`.
    pub fn describe(&self) -> String {
        let names: Vec<&str> = self.tiers.iter().map(|t| t.name()).collect();
        names.join("+")
    }

    fn backfill(&self, key: &str, entry: &CacheEntry, missed: &[usize]) {
        let mut after = AfterResponse::new();
        for &index in missed {
            after.push(self.tiers[index].promote(key.to_string(), entry.clone()));
        }
        after.drain_spawn();
    }

    async fn refresh_pin_layers(&self, key: &str) -> Result<(), String> {
        let tier = self.tiers.last().ok_or("pin cache chain is empty")?;
        let entry = tier.get_pin_entry(key).await
            .ok_or("claimed pin entry disappeared from its backend")?;
        for earlier in &self.tiers[..self.tiers.len() - 1] {
            earlier.revalidate(key.to_string(), entry.clone(), entry.media.cache.clone()).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use crate::cache::{memory::MemoryCacheBackend, sqlite::SqliteCacheBackend, unix_now_secs};

    struct ObservedBackend {
        backend: Arc<dyn CacheBackend>,
        reads: AtomicUsize,
        started: Arc<Semaphore>,
        completed: Arc<Semaphore>,
        gate: Option<Arc<Semaphore>>,
    }

    impl ObservedBackend {
        fn new(backend: Arc<dyn CacheBackend>, blocked: bool) -> Arc<Self> {
            Arc::new(Self {
                backend,
                reads: AtomicUsize::new(0),
                started: Arc::new(Semaphore::new(0)),
                completed: Arc::new(Semaphore::new(0)),
                gate: blocked.then(|| Arc::new(Semaphore::new(0))),
            })
        }

        async fn wait(semaphore: &Semaphore) {
            tokio::time::timeout(Duration::from_secs(2), semaphore.acquire())
                .await.expect("background promotion timed out").unwrap().forget();
        }
    }

    impl CacheBackend for ObservedBackend {
        fn name(&self) -> &'static str { self.backend.name() }

        fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.backend.get_entry(key, pin_ttl)
        }

        fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.backend.get_pin_entry(key)
        }

        fn promote(&self, key: String, entry: CacheEntry) -> DeferredFuture {
            let write = self.backend.promote(key, entry);
            let started = self.started.clone();
            let completed = self.completed.clone();
            let gate = self.gate.clone();
            Box::pin(async move {
                started.add_permits(1);
                if let Some(gate) = gate {
                    gate.acquire().await.unwrap().forget();
                }
                write.await;
                completed.add_permits(1);
            })
        }

        fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> DeferredFuture {
            self.backend.revalidate(key, entry, expected_cache)
        }

        fn put(&self, key: String, media: ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture {
            self.backend.put(key, media, cost, cache_until, pin_ttl)
        }
    }

    fn memory() -> Arc<dyn CacheBackend> {
        Arc::new(MemoryCacheBackend::with_max_entries(100).unwrap())
    }

    fn entry() -> CacheEntry {
        let now = unix_now_secs();
        CacheEntry::new(ThumbMedia {
            thumbnail: vec![1, 2, 3],
            cache: crate::CacheHints::expiring_in(60).encode(60),
            url: "https://example.com/source.jpg".into(),
            ..Default::default()
        }, 73, now + 300, 600, now)
    }

    #[tokio::test]
    async fn sqlite_hit_backfills_memory_without_waiting_or_renewing_deadlines() {
        let memory = ObservedBackend::new(memory(), true);
        let sqlite = ObservedBackend::new(Arc::new(SqliteCacheBackend::open(":memory:").unwrap()), false);
        let original = entry();
        sqlite.backend.promote("key".into(), original.clone()).await;
        let chain = ChainCacheBackend::new(vec![memory.clone(), sqlite.clone()]);
        let hit = tokio::time::timeout(Duration::from_secs(2), chain.get("key", 0))
            .await.expect("lookup waited for blocked promotion").unwrap();
        assert_eq!(hit.thumbnail, original.media.thumbnail);
        ObservedBackend::wait(&memory.started).await;
        assert!(memory.backend.get("key", 0).await.is_none());
        memory.gate.as_ref().unwrap().add_permits(1);
        ObservedBackend::wait(&memory.completed).await;
        let copy = memory.backend.get_entry("key", 0).await.unwrap();
        assert_eq!(copy.cache_until, original.cache_until);
        assert_eq!(copy.fresh_until, original.fresh_until);
        assert_eq!(copy.pin_until, original.pin_until);
        assert_eq!(copy.render_cost, original.render_cost);
        assert!(copy.media.url.is_empty());
        assert!(chain.get("key", 0).await.is_some());
        assert_eq!(sqlite.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn backfills_all_earlier_misses_independently_and_leaves_later_tiers_alone() {
        let first = ObservedBackend::new(memory(), true);
        let second = ObservedBackend::new(Arc::new(SqliteCacheBackend::open(":memory:").unwrap()), false);
        let source = ObservedBackend::new(memory(), false);
        let later = ObservedBackend::new(memory(), false);
        source.backend.promote("key".into(), entry()).await;
        let chain = ChainCacheBackend::new(vec![first.clone(), second.clone(), source.clone(), later.clone()]);
        assert!(chain.get("key", 0).await.is_some());
        ObservedBackend::wait(&second.completed).await;
        assert!(second.backend.get("key", 0).await.is_some());
        assert!(first.backend.get("key", 0).await.is_none());
        assert_eq!(later.reads.load(Ordering::SeqCst), 0);
        assert_eq!(source.started.available_permits(), 0);
        assert_eq!(later.started.available_permits(), 0);
        first.gate.as_ref().unwrap().add_permits(1);
        ObservedBackend::wait(&first.completed).await;
    }

    #[tokio::test]
    async fn pin_hits_backfill_without_extending_pin_or_enabling_cache_lookups() {
        let destination = ObservedBackend::new(memory(), false);
        let source = memory();
        let mut original = entry();
        original.cache_until = 0;
        original.set_cache(String::new());
        source.promote("pin".into(), original.clone()).await;
        let chain = ChainCacheBackend::new(vec![destination.clone(), source]);
        assert!(chain.get_pin("pin").await.is_some());
        ObservedBackend::wait(&destination.completed).await;
        let copy = destination.backend.get_pin_entry("pin").await.unwrap();
        assert_eq!(copy.pin_until, original.pin_until);
        assert_eq!(copy.cache_until, 0);
        assert_eq!(copy.render_cost, original.render_cost);
        assert!(destination.backend.get("pin", 0).await.is_none());
    }

    #[tokio::test]
    async fn cache_hits_promote_the_refreshed_pin_deadline() {
        let destination = ObservedBackend::new(memory(), false);
        let source = memory();
        source.promote("key".into(), entry()).await;
        let chain = ChainCacheBackend::new(vec![destination.clone(), source.clone()]);
        let hit = chain.get_entry("key", 1200).await.unwrap();
        ObservedBackend::wait(&destination.completed).await;
        let copy = destination.backend.get_entry("key", 0).await.unwrap();
        assert_eq!(copy.pin_until, hit.pin_until);
        assert_eq!(copy.pin_until, source.get_entry("key", 0).await.unwrap().pin_until);
    }

    #[tokio::test]
    async fn misses_do_not_schedule_backfill_and_definition_order_is_preserved() {
        let first = ObservedBackend::new(Arc::new(SqliteCacheBackend::open(":memory:").unwrap()), false);
        let second = ObservedBackend::new(memory(), false);
        let chain = ChainCacheBackend::new(vec![first.clone(), second.clone()]);
        assert!(chain.get("missing", 0).await.is_none());
        assert!(chain.get_pin("missing").await.is_none());
        assert_eq!(first.started.available_permits(), 0);
        assert_eq!(second.started.available_permits(), 0);
        let mut disk_entry = entry();
        disk_entry.media.thumbnail = vec![7];
        first.backend.promote("key".into(), disk_entry).await;
        second.backend.promote("key".into(), entry()).await;
        assert_eq!(chain.describe(), "sqlite+memory");
        assert_eq!(chain.get("key", 0).await.unwrap().thumbnail, vec![7]);
        assert_eq!(second.reads.load(Ordering::SeqCst), 2);
    }
}

impl CacheBackend for ChainCacheBackend {
    fn name(&self) -> &'static str {
        "chain"
    }

    fn pin_candidate(&self, kind: crate::media::FileKind, key: &str, attempt: u32) -> Result<String, String> {
        self.tiers.last().ok_or("pin cache chain is empty")?.pin_candidate(kind, key, attempt)
    }

    fn untracked_pin<'a>(&'a self, kind: crate::media::FileKind, key: &'a str) -> PinFuture<'a, String> {
        Box::pin(async move {
            self.tiers.last().ok_or("pin cache chain is empty")?.untracked_pin(kind, key).await
        })
    }

    fn issue_pin<'a>(&'a self, kind: crate::media::FileKind, key: &'a str, ttl: u64) -> PinFuture<'a, Option<String>> {
        Box::pin(async move {
            let tier = self.tiers.last().ok_or("pin cache chain is empty")?;
            let id = tier.issue_pin(kind, key, ttl).await?;
            if id.is_some() {
                self.refresh_pin_layers(key).await?;
            }
            Ok(id)
        })
    }

    fn claim_pin<'a>(&'a self, id: &'a str, key: &'a str, ttl: u64) -> PinFuture<'a, PinClaim> {
        Box::pin(async move {
            let tier = self.tiers.last().ok_or("pin cache chain is empty")?;
            let claim = tier.claim_pin(id, key, ttl).await?;
            if claim == PinClaim::Claimed {
                self.refresh_pin_layers(key).await?;
            }
            Ok(claim)
        })
    }

    fn pin_data<'a>(&'a self, id: &'a str) -> PinFuture<'a, Option<crate::http_cache::PinnedThumbnail>> {
        Box::pin(async move {
            let tier = self.tiers.last().ok_or("pin cache chain is empty")?;
            tier.pin_data(id).await
        })
    }

    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        Box::pin(async move {
            let mut hit = None;
            let mut missed = Vec::new();
            for (index, tier) in self.tiers.iter().enumerate() {
                if let Some(entry) = tier.get_entry(key, pin_ttl).await {
                    if hit.is_none() {
                        self.backfill(key, &entry, &missed);
                        hit = Some(entry);
                    }
                    if pin_ttl == 0 {
                        break;
                    }
                } else if hit.is_none() {
                    missed.push(index);
                }
            }
            hit
        })
    }

    fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        Box::pin(async move {
            let mut missed = Vec::new();
            for (index, tier) in self.tiers.iter().enumerate() {
                if let Some(entry) = tier.get_pin_entry(key).await {
                    self.backfill(key, &entry, &missed);
                    return Some(entry);
                }
                missed.push(index);
            }
            None
        })
    }

    fn promote(&self, key: String, entry: CacheEntry) -> DeferredFuture {
        let tiers = self.tiers.clone();
        Box::pin(async move {
            for tier in tiers {
                tier.promote(key.clone(), entry.clone()).await;
            }
        })
    }

    fn put(&self, key: String, media: ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture {
        let tiers = self.tiers.clone();
        Box::pin(async move {
            for tier in tiers.iter() {
                // Each tier returns an already-deferred write; run them in
                // order.  Backends swallow their own errors.
                tier.put(key.clone(), media.clone(), cost, cache_until, pin_ttl).await;
            }
        })
    }

    fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> DeferredFuture {
        let tiers = self.tiers.clone();
        Box::pin(async move {
            for tier in tiers {
                tier.revalidate(key.clone(), entry.clone(), expected_cache.clone()).await;
            }
        })
    }
}
