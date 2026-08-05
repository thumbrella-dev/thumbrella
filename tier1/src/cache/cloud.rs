//! Cloud-service cache backend.
//!
//! Forwards cache lookups and stores to the Thumbrella cloud service
//! (`/cache/lookup` and `/cache/store` endpoints).  This lets a private
//! server use the cloud as a distributed, shared cache layer.
//!
//! ## DSN format
//!
//! `cloud:<connect-string>` — same grammar as `TBR_CONNECT` / `TBR_TIER2`:
//! - `cloud:tbr_e_xxx` — bare auth token, uses default cloud host
//! - `cloud:https://cloud.thumbrella.dev,tbr_e_xxx` — explicit host + token
//! - `cloud:http://localhost:8787,tbr_s_xxx` — local / beta server
//!
//! ## Key model
//!
//! The cloud derives cache keys from the source URL + the auth token's
//! account_id.  This backend sends the URL and lets the cloud own the key
//! namespace — the standalone server never sees or supplies account-scoped
//! keys.
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

impl CacheBackend for CloudCacheBackend {
    fn name(&self) -> &'static str {
        "cloud"
    }

    fn get<'a>(&'a self, source_url: &'a str) -> Pin<Box<dyn Future<Output = Option<crate::result::ThumbMedia>> + Send + 'a>> {
        let url = format!("{}/cache/lookup", self.base_url);
        let auth = self.auth_header.clone();
        let body = serde_json::json!({"url": source_url}).to_string();
        let client = self.client.clone();
        Box::pin(async move {
            let resp = client
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .ok()?;

            if !resp.status().is_success() {
                return None;
            }

            let json: serde_json::Value = resp.json().await.ok()?;

            if json.get("status").and_then(|v| v.as_str()) == Some("miss") {
                return None;
            }

            serde_json::from_value::<crate::result::ThumbMedia>(json).ok()
        })
    }

    fn put(&self, _key: String, media: crate::result::ThumbMedia, _cost: u8, _expires_at: u64) -> DeferredFuture {
        let url = format!("{}/cache/store", self.base_url);
        let auth = self.auth_header.clone();
        let client = self.client.clone();

        let Ok(body) = serde_json::to_string(&media) else {
            return Box::pin(async {});
        };

        Box::pin(async move {
            let _ = client
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await;
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
    let resp = client
        .post(&url)
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .body(r#"{"url":"https://thumbrella.dev/ping"}"#)
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
