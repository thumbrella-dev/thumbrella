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
//!   request: { "url": "<canonical source url>", "cache_format": <u32> }
//!   hit:     200 <ThumbMedia json, url restored>
//!   miss:    200 { "status": "miss", "url": "<url>" }
//!
//! POST /cache/store
//!   request: { "url": "<canonical source url>",
//!              "cache_format": <u32>,
//!              "retain_secs": <u64>,
//!              "media": <ThumbMedia json, url omitted> }
//!   success: 200 { "status": "stored", "url": "<url>", "ttl_secs": <u64> }
//! ```
//!
//! `cache_format` is [`crate::TBR_CACHE_VERSION`]: the format this build writes.
//! The cloud salts it into the storage key, so two incompatible server versions
//! sharing one account never read each other's entries.  Omitting it means "the
//! format the serving build speaks" - see the worker's request types.
//!
//! ## Retention is not freshness
//!
//! `retain_secs` comes from `put`'s `expires_at` and says how long the cloud
//! should *keep* the entry.  It is deliberately independent of the freshness
//! metadata inside `media.cache`, which the cloud stores and returns verbatim.
//!
//! The two must not be conflated.  A stale entry is still valuable: it carries
//! the validators (`etag` / `last-modified`) that let the pipeline revalidate
//! with a conditional request and reuse the stored thumbnail on `304`.  That is
//! why an entry whose freshness window has already closed is still worth
//! retaining, and why retention must never be derived from `media.cache`.
//! The cloud caps retention at the account plan's maximum and rejects only
//! writes with no retention left at all.
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
use crate::cache::CacheBackend;
use crate::connect::ConnectTarget;

/// Default cloud service host.
const DEFAULT_CLOUD_HOST: &str = "https://cloud.thumbrella.dev";

/// Cache backend that delegates to the Thumbrella cloud service.
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

impl CacheBackend for CloudCacheBackend {
    fn name(&self) -> &'static str {
        "cloud"
    }

    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<crate::result::ThumbMedia>> + Send + 'a>> {
        debug_assert!(
            is_source_identity(key),
            "cloud cache addresses entries by source identity, but got an opaque key {key:?}",
        );
        let url = format!("{}/cache/lookup", self.base_url);
        let auth = self.auth_header.clone();
        let body = serde_json::json!({
            "url": key,
            "cache_format": crate::TBR_CACHE_VERSION,
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

            match serde_json::from_value::<crate::result::ThumbMedia>(json) {
                Ok(media) => Some(media),
                Err(e) => {
                    tracing::warn!("cloud cache: lookup for {key} returned an unusable entry - {e}");
                    None
                }
            }
        })
    }

    fn put(&self, key: String, media: crate::result::ThumbMedia, _cost: u8, expires_at: u64) -> DeferredFuture {
        debug_assert!(
            is_source_identity(&key),
            "cloud cache addresses entries by source identity, but got an opaque key {key:?}",
        );
        let url = format!("{}/cache/store", self.base_url);
        let auth = self.auth_header.clone();
        let client = self.client.clone();

        // Retention is the window the caller asked for.  Nothing to retain
        // means nothing to write - the cloud is never asked to guess a TTL
        // from the freshness metadata.
        let now = web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let Some(retain_secs) = expires_at.checked_sub(now).filter(|secs| *secs > 0) else {
            tracing::debug!("cloud cache: skipping store for {key} - retention window already elapsed");
            return Box::pin(async {});
        };

        let mut media = media;
        media.url.clear();
        let Ok(body) = serde_json::to_string(&StoreRequest {
            url: &key,
            cache_format: crate::TBR_CACHE_VERSION,
            retain_secs,
            media: &media,
        }) else {
            tracing::warn!("cloud cache: could not serialise the entry for {key}");
            return Box::pin(async {});
        };

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
                    tracing::warn!("cloud cache: store for {key} failed - {e}");
                    return;
                }
            };

            let status = resp.status();
            if status.is_success() {
                tracing::debug!("cloud cache: stored {key} for {retain_secs}s");
                return;
            }

            let text = resp.text().await.unwrap_or_default();
            tracing::warn!("cloud cache: store for {key} rejected with HTTP {status} - {}", snippet(&text));
        })
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
