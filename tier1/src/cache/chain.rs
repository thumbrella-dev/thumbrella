//! Layered cache chain - read through tiers in order, write through to all.
//!
//! A chain is a list of backends (L1, L2, ...) selected by a `TBR_CACHE`
//! spec such as `mem:64mb+sqlite:/var/cache.db,1gb`.  Tier order is the
//! order written, fastest first.
//!
//! ## Policy
//!
//! - `get` consults each tier in order and returns the first hit.
//! - `put` writes through to every tier, so each layer is populated together.
//!
//! Read-hits are intentionally *not* promoted back into earlier tiers: the
//! backend trait and storage do not carry an entry's original `expires_at`
//! across a read, so a faithful re-insert is impossible and write-through
//! already keeps the front tiers warm in steady state.  The short sticky
//! frontend above `CacheStore` handles burst de-duplication regardless.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::after::DeferredFuture;
use crate::cache::CacheBackend;
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
}

impl CacheBackend for ChainCacheBackend {
    fn name(&self) -> &'static str {
        "chain"
    }

    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<ThumbMedia>> + Send + 'a>> {
        let tiers = self.tiers.clone();
        Box::pin(async move {
            for tier in tiers.iter() {
                if let Some(media) = tier.get(key).await {
                    return Some(media);
                }
            }
            None
        })
    }

    fn put(&self, key: String, media: ThumbMedia, cost: u8, expires_at: u64) -> DeferredFuture {
        let tiers = self.tiers.clone();
        Box::pin(async move {
            for tier in tiers.iter() {
                // Each tier returns an already-deferred write; run them in
                // order.  Backends swallow their own errors.
                tier.put(key.clone(), media.clone(), cost, expires_at).await;
            }
        })
    }
}
