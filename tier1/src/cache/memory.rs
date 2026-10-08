//! In-memory LRU cache backend.
//!
//! Bounded by total approximate byte size or entry count.
//! Lookups enforce independent cache and pin deadlines. Fully expired entries
//! are removed on reads and writes; capacity eviction remains managed by Moka.
//!
//! ## Spec format
//!
//! `mem:` with an optional size (as a single link in a `TBR_CACHE` chain):
//! - `mem:`        - default 100 MB
//! - `mem:500`     - max 500 entries
//! - `mem:200mb`   - max 200 MB
//! - `mem:2gb`     - max 2 GB

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::collections::HashMap;

use crate::after::DeferredFuture;
use crate::cache::{CacheBackend, CacheEntry, unix_now_secs};
use crate::cache::{PinFuture, pins::{PinClaim, PinSecret}};
use parking_lot::Mutex;

pub struct MemoryCacheBackend {
    cache: Arc<Mutex<moka::sync::Cache<String, CacheEntry>>>,
    pins: Arc<Mutex<HashMap<String, String>>>,
    pin_secret: PinSecret,
}

impl MemoryCacheBackend {
    pub fn with_max_bytes(max_bytes: u64) -> Result<Self, String> {
        let pin_secret = PinSecret::generate().map_err(|e| format!("cannot generate pin secret: {e}"))?;
        let max_bytes = max_bytes.max(1024 * 1024);
        let cap_hint = (max_bytes / 512).min(100_000);
        let cache = moka::sync::Cache::builder()
            .support_invalidation_closures()
            .max_capacity(cap_hint.max(1))
            .weigher(|_key: &String, value: &CacheEntry| -> u32 {
                let size = value.media.thumbnail.len()
                    + value.media.cache.len()
                    + value.media.properties.to_string().len()
                    + 512;
                size.min(u32::MAX as usize) as u32
            })
            .build();
        Ok(Self { cache: Arc::new(Mutex::new(cache)), pins: Default::default(), pin_secret })
    }

    pub fn with_max_entries(max_entries: u64) -> Result<Self, String> {
        let pin_secret = PinSecret::generate().map_err(|e| format!("cannot generate pin secret: {e}"))?;
        let max_entries = max_entries.max(10);
        let cache = moka::sync::Cache::builder().support_invalidation_closures().max_capacity(max_entries).build();
        Ok(Self { cache: Arc::new(Mutex::new(cache)), pins: Default::default(), pin_secret })
    }

    pub fn default_cache() -> Result<Self, String> {
        Self::with_max_bytes(100 * 1024 * 1024)
    }

    fn lookup(&self, key: &str, pin: bool, pin_ttl: u64) -> Option<CacheEntry> {
        let cache = self.cache.lock();
        let mut entry = cache.get(key)?;
        let now = unix_now_secs();
        let hit = entry.access(now, pin, pin_ttl);
        if entry.retain_until() <= now {
            cache.invalidate(key);
        } else if hit {
            cache.insert(key.to_string(), entry.clone());
        }
        hit.then_some(entry)
    }
}

impl CacheBackend for MemoryCacheBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn pin_candidate(&self, kind: crate::media::FileKind, key: &str, attempt: u32) -> Result<String, String> {
        Ok(self.pin_secret.candidate(kind, key, attempt))
    }

    fn claim_pin<'a>(&'a self, id: &'a str, key: &'a str, ttl: u64) -> PinFuture<'a, PinClaim> {
        Box::pin(async move {
            let cache = self.cache.lock();
            let now = unix_now_secs();
            let mut pins = self.pins.lock();
            pins.retain(|_, key| cache.get(key).is_some_and(|entry| entry.pin_until > now));
            if pins.get(id).is_some_and(|owner| owner != key) {
                return Ok(PinClaim::Conflict);
            }
            let Some(mut entry) = cache.get(key).filter(|entry| entry.retain_until() > now) else {
                return Ok(PinClaim::Missing);
            };
            entry.pin_until = entry.pin_until.max(now.saturating_add(ttl));
            cache.insert(key.to_string(), entry);
            pins.insert(id.to_string(), key.to_string());
            Ok(PinClaim::Claimed)
        })
    }

    fn pin_thumbnail<'a>(&'a self, id: &'a str) -> PinFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let cache = self.cache.lock();
            let pins = self.pins.lock();
            Ok(pins.get(id).and_then(|key| cache.get(key))
                .filter(|entry| entry.pin_until > unix_now_secs())
                .map(|entry| entry.media.thumbnail))
        })
    }

    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        Box::pin(async move { self.lookup(key, false, pin_ttl) })
    }

    fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        Box::pin(async move { self.lookup(key, true, 0) })
    }

    fn promote(&self, key: String, mut entry: CacheEntry) -> DeferredFuture {
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let now = unix_now_secs();
            let cache = cache.lock();
            if entry.retain_until() <= now
                || cache.get(&key).is_some_and(|existing| existing.retain_until() > now)
            {
                return;
            }
            entry.media.url.clear();
            cache.insert(key, entry);
        })
    }

    fn revalidate(&self, key: String, mut entry: CacheEntry, expected_cache: String) -> DeferredFuture {
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let cache = cache.lock();
            if let Some(existing) = cache.get(&key) {
                if existing.media.cache != expected_cache {
                    return;
                }
                entry.pin_until = entry.pin_until.max(existing.pin_until);
            }
            entry.media.url.clear();
            if entry.retain_until() <= unix_now_secs() {
                cache.invalidate(&key);
            } else {
                cache.insert(key, entry);
            }
        })
    }

    fn put(&self, key: String, media: crate::result::ThumbMedia, cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture {
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let now = unix_now_secs();
            let cache = cache.lock();
            let mut entry = CacheEntry::new(media, cost, cache_until, pin_ttl, now);
            if let Some(existing) = cache.get(&key) {
                entry.pin_until = entry.pin_until.max(existing.pin_until);
            }
            if entry.retain_until() <= now {
                cache.invalidate(&key);
                return;
            }
            cache.invalidate_entries_if(move |_, entry| entry.retain_until() <= now)
                .expect("memory cache invalidation is enabled");
            cache.insert(key, entry);
        })
    }
}

/// Parse the size portion of a `mem:` DSN fragment.
pub fn parse_mem_size(fragment: &str) -> Result<Option<(u64, &str)>, String> {
    let spec = fragment.trim();
    if spec.is_empty() {
        return Ok(None);
    }

    let lower = spec.to_ascii_lowercase();

    if let Some(num) = lower.strip_suffix("gb") {
        let gb: f64 = num
            .trim()
            .parse()
            .map_err(|_| format!("invalid size spec: '{spec}' - expected a number before 'gb'"))?;
        if gb <= 0.0 || gb > 1024.0 {
            return Err(format!("GB size must be between 0 and 1024, got {gb}"));
        }
        return Ok(Some(((gb * 1024.0 * 1024.0 * 1024.0) as u64, "bytes")));
    }

    if let Some(num) = lower.strip_suffix("mb") {
        let mb: f64 = num
            .trim()
            .parse()
            .map_err(|_| format!("invalid size spec: '{spec}' - expected a number before 'mb'"))?;
        if mb <= 0.0 || mb > (1024.0 * 1024.0) {
            return Err(format!("MB size must be between 0 and 1,048,576, got {mb}"));
        }
        return Ok(Some(((mb * 1024.0 * 1024.0) as u64, "bytes")));
    }

    if let Some(num) = lower.strip_suffix("kb") {
        let kb: f64 = num
            .trim()
            .parse()
            .map_err(|_| format!("invalid size spec: '{spec}' - expected a number before 'kb'"))?;
        if kb <= 0.0 {
            return Err(format!("KB size must be positive, got {kb}"));
        }
        return Ok(Some(((kb * 1024.0) as u64, "bytes")));
    }

    let count: u64 = spec
        .trim()
        .parse()
        .map_err(|_| format!("invalid size spec: '{spec}' - expected a number, or number+unit (mb/gb/kb)"))?;
    if count < 10 {
        return Err(format!("entry count must be at least 10, got {count}"));
    }
    if count > 100_000_000 {
        return Err(format!("entry count must be at most 100,000,000, got {count}"));
    }
    Ok(Some((count, "entries")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mem_size() {
        assert_eq!(parse_mem_size("").unwrap(), None);
        assert_eq!(parse_mem_size("100mb").unwrap(), Some((100 * 1024 * 1024, "bytes")));
        assert_eq!(parse_mem_size("2gb").unwrap(), Some((2 * 1024 * 1024 * 1024, "bytes")));
        assert_eq!(parse_mem_size("500").unwrap(), Some((500, "entries")));
        assert!(parse_mem_size("abc").is_err());
        assert!(parse_mem_size("0gb").is_err());
    }
}
