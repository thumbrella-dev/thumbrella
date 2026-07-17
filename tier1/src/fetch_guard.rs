//! Fetch-protection caches — URL failure debounce and origin back-off.
//!
//! Both types are cheaply cloneable (inner data is reference-counted) so they
//! can live on [`crate::cook::Runtime`] and be shared across all concurrent
//! cooks without extra allocation.
//!
//! # Platform split
//! * **native** — backed by [`moka::future::Cache`]: bounded capacity,
//!   thread-safe, background TTL eviction.
//! * **wasm32** — backed by [`parking_lot::RwLock`] over a `HashMap`:
//!   lazy TTL expiry on read, no background threads required.

use std::sync::Arc;
use web_time::{Duration, Instant};

//  UrlFailureCache

/// Short-lived debounce cache for URLs that recently returned 4xx / 5xx.
///
/// Prevents re-fetching a URL that just returned an error within the debounce
/// window. TTL: 5 s flat. Value: `(http_status, error_message)`.
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct UrlFailureCache(moka::future::Cache<String, (u16, Arc<str>)>);

#[cfg(not(feature = "native"))]
#[derive(Clone)]
pub struct UrlFailureCache(
    Arc<parking_lot::RwLock<std::collections::HashMap<String, ((u16, Arc<str>), Instant)>>>,
    u64,
);

impl UrlFailureCache {
    pub fn new(ttl_secs: u64) -> Self {
        #[cfg(feature = "native")]
        return Self(
            moka::future::Cache::builder()
                .max_capacity(4_096)
                .time_to_live(Duration::from_secs(ttl_secs))
                .build(),
        );

        #[cfg(not(feature = "native"))]
        return Self(Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())), ttl_secs);
    }

    /// Returns `(status, message)` if this URL is within the failure window.
    pub async fn check(&self, url: &str) -> Option<(u16, Arc<str>)> {
        #[cfg(feature = "native")]
        return self.0.get(url).await;

        #[cfg(not(feature = "native"))]
        {
            let now = Instant::now();
            self.0.read().get(url).and_then(
                |((s, m), exp)| {
                    if now < *exp { Some((*s, m.clone())) } else { None }
                },
            )
        }
    }

    /// Record a URL fetch failure.
    pub async fn record(&self, url: String, status: u16, message: Arc<str>) {
        #[cfg(feature = "native")]
        {
            self.0.insert(url, (status, message)).await;
        }

        #[cfg(not(feature = "native"))]
        {
            let exp = Instant::now() + Duration::from_secs(self.1);
            self.0.write().insert(url, ((status, message), exp));
        }
    }
}

//  OriginBackoffCache

/// Rate-control cache for origins that returned 429 / 503.
///
/// Keyed by `scheme://host[:port]`. TTL is variable — taken from the upstream
/// `Retry-After` header (integer seconds), falling back to
/// `Runtime::backoff_default`. Value: the HTTP status code that triggered
/// the back-off.
#[cfg(feature = "native")]
#[derive(Clone)]
pub struct OriginBackoffCache(moka::future::Cache<String, (u16, Instant)>);

#[cfg(not(feature = "native"))]
#[derive(Clone)]
pub struct OriginBackoffCache(Arc<parking_lot::RwLock<std::collections::HashMap<String, (u16, Instant)>>>);

impl OriginBackoffCache {
    #[cfg_attr(not(feature = "native"), allow(unused_variables))]
    pub fn new(ceiling_secs: u64) -> Self {
        #[cfg(feature = "native")]
        return Self(
            moka::future::Cache::builder()
                .max_capacity(1_024)
                .time_to_live(Duration::from_secs(ceiling_secs))
                .build(),
        );

        #[cfg(not(feature = "native"))]
        return Self(Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())));
    }

    /// Returns the HTTP status code that triggered the back-off, if still active.
    pub async fn check(&self, origin: &str) -> Option<u16> {
        #[cfg(feature = "native")]
        {
            let (status, exp) = self.0.get(origin).await?;
            if Instant::now() < exp { Some(status) } else { None }
        }

        #[cfg(not(feature = "native"))]
        {
            let now = Instant::now();
            self.0
                .read()
                .get(origin)
                .and_then(|(s, exp)| if now < *exp { Some(*s) } else { None })
        }
    }

    /// Record an origin back-off.
    ///
    /// `ttl_secs` comes from the upstream `Retry-After` header; pass
    /// `runtime.backoff_default` when the header is absent.
    pub async fn record(&self, origin: String, status: u16, ttl_secs: u64) {
        let exp = Instant::now() + Duration::from_secs(ttl_secs);
        #[cfg(feature = "native")]
        {
            self.0.insert(origin, (status, exp)).await;
        }

        #[cfg(not(feature = "native"))]
        {
            self.0.write().insert(origin, (status, exp));
        }
    }
}
