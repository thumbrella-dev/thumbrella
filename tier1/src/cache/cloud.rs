//! Cloud-service cache backend.
//!
//! Delegates cache reads and writes to the Thumbrella cloud service, letting a
//! private server use the cloud as a distributed, shared cache layer.
//!
//! ## DSN format
//!
//! `cloud:<connect-string>` — same grammar as `TBR_CONNECT` / `TBR_TIER2`:
//! - `cloud:tbr_s_xxx` — bare auth token, uses default cloud host
//! - `cloud:https://cloud.thumbrella.dev,tbr_s_xxx` — explicit host + token
//! - `cloud:http://localhost:8787,tbr_s_xxx` — local / beta server
//!
//! ## Who owns the cache key
//!
//! The cloud owns it.  Entries are addressed by the **canonical source URL**;
//! the cloud hashes that into a storage key scoped to the auth token's account,
//! with the writer's cache format version salted in.  A standalone server has no
//! account of its own, so it never computes or transmits an account-scoped key -
//! it sends the source URL and lets the cloud do the namespacing.
//!
//! Because of that, the `key` handed to this backend must *be* the canonical
//! source identity.  That holds for every deployment able to configure `cloud:`:
//! only a standalone server can, and it has no account, so
//! `ThumbCook::ctx_cache_key` is `None` and the pipeline keys by identity.
//! `is_source_identity` asserts it in debug builds, and a violation now fails
//! loudly at the endpoint instead of silently missing.
//!
//! ## Protocol
//!
//! ```text
//! POST /cache/lookup
//!   request: { "url": "<canonical source url>", "cache_format": <u32>,
//!              "pin_ttl_secs": <u64, defaults to 0> }
//!   hit:     200 <ThumbMedia json, url restored>
//!   miss:    200 { "status": "miss", "url": "<url>" }
//!   internal callers add "include_entry": true to receive CacheEntry metadata
//!   with absolute deadlines and render cost, with the media URL still omitted
//!
//! POST /cache/pin
//!   request and response: same as lookup, but checks pin expiration only
//!   and does not extend it. This is authenticated backend access, not a
//!   public pin URL endpoint.
//!
//! POST /cache/store
//!   request: { "url": "<canonical source url>",
//!              "cache_format": <u32>,
//!              "retain_secs": <u64>,
//!              "pin_ttl_secs": <u64, defaults to 0>,
//!              "media": <ThumbMedia json, url omitted> }
//!   success: 200 { "status": "stored", "url": "<url>", "ttl_secs": <u64> }
//! ```
//!
//! Backfill writes set the relative TTLs to zero and supply `promotion` with
//! `fresh_until`, `cache_until`, `pin_until`, `last_accessed_at`, and `render_cost`. The cloud
//! caps, but never extends, these deadlines and skips existing live entries.
//! Conditional insertion is best-effort across cloud isolates.
//! Revalidation supplies the same metadata plus `expected_cache`; it replaces
//! only that stored representation, preserving pin lifetime and render cost.
//!
//! `cache_format` is [`crate::TBR_CACHE_VERSION`]: the format this build writes.
//! The cloud salts it into the storage key, so two incompatible server versions
//! sharing one account never read each other's entries.  Omitting it means "the
//! format the serving build speaks" - see the worker's request types.
//!
//! ## Retention is not freshness
//!
//! `retain_secs` comes from `put`'s `cache_until` and controls cache lookups.
//! Physical retention uses the longer cache or pin lifetime. It is independent of the freshness
//! metadata inside `media.cache`, which the cloud stores and returns verbatim.
//!
//! The two must not be conflated.  A stale entry is still valuable: it carries
//! the validators (`etag` / `last-modified`) that let the pipeline revalidate
//! with a conditional request and reuse the stored thumbnail on `304`.  That is
//! why an entry whose freshness window has already closed is still worth
//! retaining, and why retention must never be derived from `media.cache`.
//! The cloud caps retention at the account plan's maximum and rejects only
//! writes with neither cache retention nor pin lifetime. Pin refreshes in KV
//! are best-effort read/modify/write operations, not cross-isolate transactions.
//!
//! ## Health check
//!
//! [`ping_cloud_backend`] sends a dummy `/cache/lookup` to verify the token
//! and endpoint.  Called by the `tier1 check` subcommand, NOT at server
//! startup - construction never blocks on network I/O.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::after::DeferredFuture;
use crate::cache::{CacheBackend, CacheEntry, CacheEntryMetadata, unix_now_secs};
use crate::connect::ConnectTarget;

/// Default cloud service host.
const DEFAULT_CLOUD_HOST: &str = "https://cloud.thumbrella.dev";

/// Cache backend that delegates to the Thumbrella cloud service.
#[derive(Clone)]
pub struct CloudCacheBackend {
    base_url: String,
    auth_header: String,
    client: reqwest::Client,
}

impl CloudCacheBackend {
    /// Create a new cloud cache backend from a parsed [`ConnectTarget`].
    /// Does NOT perform a health check - use [`ping_cloud_backend`] for
    /// upfront validation.
    pub fn new(target: ConnectTarget) -> Result<Self, String> {
        let base_url = target.url.unwrap_or_else(|| DEFAULT_CLOUD_HOST.to_string());
        if base_url.is_empty() {
            return Err("cloud cache: URL is empty".to_string());
        }

        let auth_header = target
            .headers
            .get("Authorization")
            .cloned()
            .unwrap_or_default();
        if auth_header.is_empty() {
            return Err("cloud cache: no Authorization header found in connect string".to_string());
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("cloud cache: failed to create HTTP client: {e}"))?;

        Ok(Self {
            base_url,
            auth_header,
            client,
        })
    }
}

/// Body of `POST /cache/store`.
#[derive(serde::Serialize)]
struct StoreRequest<'a> {
    /// Canonical source URL.  The cloud derives the storage key from this.
    url: &'a str,
    /// The writer's cache format version - see [`crate::TBR_CACHE_VERSION`].
    cache_format: u32,
    /// Retention window in seconds - how long the cloud should keep the entry.
    retain_secs: u64,
    pin_ttl_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    promotion: Option<CacheEntryMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_cache: Option<String>,
    /// The stored media.  Its `url` field is cleared: the key already
    /// identifies the source, so embedding it again would only leak the URL
    /// into a KV dump.
    media: &'a crate::result::ThumbMedia,
}

/// `true` when `key` looks like a source identity rather than an opaque
/// storage key.
///
/// Route validation normalises every accepted URL, so an identity always
/// carries a scheme (`https://...`, `file://...`).  The one key shape that
/// would break this backend is the cloud's account-scoped `"{account}/{hash}"`,
/// which a standalone server never produces because it has no account of its
/// own - see the type-level docs.
fn is_source_identity(key: &str) -> bool {
    key.contains("://")
}

/// Truncate a response body for logging.
fn snippet(text: &str) -> String {
    const MAX: usize = 200;
    let trimmed = text.trim();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

impl CloudCacheBackend {
    fn lookup<'a>(&'a self, key: &'a str, pin: bool, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        debug_assert!(
            is_source_identity(key),
            "cloud cache addresses entries by source identity, but got an opaque key {key:?}",
        );
        let endpoint = if pin { "pin" } else { "lookup" };
        let url = format!("{}/cache/{endpoint}", self.base_url);
        let auth = self.auth_header.clone();
        let body = serde_json::json!({
            "url": key,
            "cache_format": crate::TBR_CACHE_VERSION,
            "pin_ttl_secs": pin_ttl,
            "include_entry": true,
        })
        .to_string();
        let client = self.client.clone();
        Box::pin(async move {
            let resp = match client
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!("cloud cache: lookup for {key} failed - {e}");
                    return None;
                }
            };

            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                tracing::warn!("cloud cache: lookup for {key} returned HTTP {status} - {}", snippet(&text));
                return None;
            }

            let json: serde_json::Value = match serde_json::from_str(&text) {
                Ok(json) => json,
                Err(e) => {
                    tracing::warn!("cloud cache: lookup for {key} returned invalid JSON - {e}");
                    return None;
                }
            };

            if json.get("status").and_then(|v| v.as_str()) == Some("miss") {
                return None;
            }

            match serde_json::from_value::<CacheEntry>(json) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    tracing::warn!("cloud cache: lookup for {key} returned an unusable entry - {e}");
                    None
                }
            }
        })
    }
    fn store(&self, key: String, mut media: crate::result::ThumbMedia, cache_until: u64, pin_ttl: u64, promotion: Option<CacheEntryMetadata>, expected_cache: Option<String>) -> DeferredFuture {
        debug_assert!(
            is_source_identity(&key),
            "cloud cache addresses entries by source identity, but got an opaque key {key:?}",
        );
        let url = format!("{}/cache/store", self.base_url);
        let auth = self.auth_header.clone();
        let client = self.client.clone();

        Box::pin(async move {
            let now = unix_now_secs();
            let retain_secs = cache_until.saturating_sub(now);
            if expected_cache.is_none() && promotion.as_ref().map_or(retain_secs == 0 && pin_ttl == 0, |metadata|
                metadata.cache_until.max(metadata.pin_until) <= now)
            {
                tracing::debug!("cloud cache: skipping store for {key} - retention window already elapsed");
                return;
            }
            media.url.clear();
            let body = match serde_json::to_string(&StoreRequest {
                url: &key,
                cache_format: crate::TBR_CACHE_VERSION,
                retain_secs,
                pin_ttl_secs: pin_ttl,
                promotion,
                expected_cache,
                media: &media,
            }) {
                Ok(body) => body,
                Err(e) => {
                    tracing::warn!("cloud cache: could not serialise the entry for {key} - {e}");
                    return;
                }
            };
            let resp = match client
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!("cloud cache: store for {key} failed - {e}");
                    return;
                }
            };

            let status = resp.status();
            if status.is_success() {
                tracing::debug!("cloud cache: store accepted for {key}");
                return;
            }

            let text = resp.text().await.unwrap_or_default();
            tracing::warn!("cloud cache: store for {key} rejected with HTTP {status} - {}", snippet(&text));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[derive(Clone)]
    struct TestService {
        entry: CacheEntry,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    async fn lookup(State(service): State<TestService>, Json(body): Json<serde_json::Value>) -> Json<CacheEntry> {
        service.requests.lock().push(body);
        Json(service.entry)
    }

    async fn store(State(service): State<TestService>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        service.requests.lock().push(body);
        Json(serde_json::json!({"status": "stored"}))
    }

    #[tokio::test]
    async fn cloud_transport_carries_promotion_metadata_without_duplicate_media_or_new_ttls() {
        let now = unix_now_secs();
        let entry = CacheEntry::new(crate::result::ThumbMedia {
            cache: crate::CacheHints::expiring_in(60).encode(60),
            thumbnail: vec![1, 2, 3],
            ..Default::default()
        }, 73, now + 300, 600, now);
        let requests = Arc::new(Mutex::new(vec![]));
        let service = TestService { entry: entry.clone(), requests: requests.clone() };
        let app = Router::new()
            .route("/cache/lookup", post(lookup))
            .route("/cache/pin", post(lookup))
            .route("/cache/store", post(store))
            .with_state(service);
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let backend = CloudCacheBackend {
            base_url: format!("http://127.0.0.1:{port}"),
            auth_header: "Bearer test-token".into(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        };
        let hit = backend.get_entry("https://example.com/a.jpg", 0).await.unwrap();
        assert_eq!(hit.cache_until, entry.cache_until);
        assert_eq!(hit.fresh_until, entry.fresh_until);
        assert_eq!(hit.pin_until, entry.pin_until);
        assert_eq!(hit.render_cost, entry.render_cost);
        assert!(backend.get_pin("https://example.com/a.jpg").await.is_some());
        backend.promote("https://example.com/a.jpg".into(), hit).await;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let requests = requests.lock();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["include_entry"], true);
        assert_eq!(requests[1]["include_entry"], true);
        let promotion = &requests[2];
        assert_eq!(promotion["retain_secs"], 0);
        assert_eq!(promotion["pin_ttl_secs"], 0);
        assert_eq!(promotion["promotion"]["cache_until"], entry.cache_until);
        assert_eq!(promotion["promotion"]["fresh_until"], entry.fresh_until);
        assert_eq!(promotion["promotion"]["pin_until"], entry.pin_until);
        assert_eq!(promotion["promotion"]["render_cost"], 73);
        assert_eq!(promotion["media"]["url"], "");
        assert!(promotion["promotion"].get("media").is_none());
    }
}

impl CacheBackend for CloudCacheBackend {
    fn name(&self) -> &'static str { "cloud" }

    fn get_entry<'a>(&'a self, key: &'a str, pin_ttl: u64) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        self.lookup(key, false, pin_ttl)
    }

    fn get_pin_entry<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<CacheEntry>> + Send + 'a>> {
        self.lookup(key, true, 0)
    }

    fn promote(&self, key: String, entry: CacheEntry) -> DeferredFuture {
        let metadata = entry.metadata();
        self.store(key, entry.media, 0, 0, Some(metadata), None)
    }

    fn revalidate(&self, key: String, entry: CacheEntry, expected_cache: String) -> DeferredFuture {
        let metadata = entry.metadata();
        self.store(key, entry.media, 0, 0, Some(metadata), Some(expected_cache))
    }

    fn put(&self, key: String, media: crate::result::ThumbMedia, _cost: u8, cache_until: u64, pin_ttl: u64) -> DeferredFuture {
        if cache_until <= unix_now_secs() && pin_ttl == 0 {
            let backend = self.clone();
            return Box::pin(async move {
                if let Some(existing) = backend.get_entry(&key, 0).await {
                    let expected_cache = existing.media.cache.clone();
                    let mut entry = CacheEntry::new(media, existing.render_cost, cache_until, 0, unix_now_secs());
                    entry.pin_until = existing.pin_until;
                    backend.revalidate(key, entry, expected_cache).await;
                }
            });
        }
        self.store(key, media, cache_until, pin_ttl, None, None)
    }
}

/// Verify cloud connectivity by sending a dummy `/cache/lookup` request.
///
/// Returns `Ok(())` when the service responds with `{"status":"miss"}`.
/// Returns `Err(...)` on network errors, HTTP errors, or unexpected
/// responses.
pub async fn ping_cloud_backend(target: &ConnectTarget) -> Result<(), String> {
    let base_url = target.url.as_deref().unwrap_or(DEFAULT_CLOUD_HOST);
    let auth_header = target
        .headers
        .get("Authorization")
        .cloned()
        .unwrap_or_default();
    if auth_header.is_empty() {
        return Err("no Authorization header in connect string".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let url = format!("{base_url}/cache/lookup");
    let body = serde_json::json!({
        "url": "https://thumbrella.dev/ping",
        "cache_format": crate::TBR_CACHE_VERSION,
    })
    .to_string();
    let resp = client
        .post(&url)
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("health check failed - {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("health check returned HTTP {status} - check the auth token"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("health check response not valid JSON: {e}"))?;

    let s = body.get("status").and_then(|v| v.as_str());

    if s == Some("miss") || s == Some("ok") {
        return Ok(());
    }

    if s == Some("error") {
        let msg = body.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
        return Err(format!("health check failed - {msg}"));
    }

    Err(format!("unexpected health check response: {body}"))
}
