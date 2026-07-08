//! Cloud-service cache backend.
//!
//! Forwards cache lookups and stores to the Thumbrella cloud service
//! (`/cache/lookup` and `/cache/store` endpoints).  This lets a private
//! server use the cloud as a distributed, shared cache layer.
//!
//! ## DSN format
//!
//! `cloud:<auth-token>` - uses the default cloud host (`cloud.thumbrella.dev`).
//! The auth token is sent as `Authorization: Bearer <token>` on every request.
//!
//! ## Health check
//!
//! [`ping_cloud_token`] sends a dummy `/cache/lookup` to verify the token
//! and endpoint.  Called by the `tier1 check` subcommand, NOT at server
//! startup - construction never blocks on network I/O.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::after::DeferredFuture;
use crate::cache::CacheBackend;

/// Default cloud service host.
const DEFAULT_CLOUD_HOST: &str = "https://cloud.thumbrella.dev";

/// Cache backend that delegates to the Thumbrella cloud service.
pub struct CloudCacheBackend {
    base_url: String,
    auth_token: String,
    client: reqwest::Client,
}

impl CloudCacheBackend {
    /// Create a new cloud cache backend.  Does NOT perform a health check -
    /// use [`ping_cloud_token`] for upfront validation.
    pub fn new(auth_token: &str) -> Result<Self, String> {
        let token = auth_token.trim().to_string();
        if token.is_empty() {
            return Err("cloud cache: auth token is empty".to_string());
        }

        let base_url = DEFAULT_CLOUD_HOST.to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("cloud cache: failed to create HTTP client: {e}"))?;

        Ok(Self {
            base_url,
            auth_token: token,
            client,
        })
    }
}

impl CacheBackend for CloudCacheBackend {
    fn name(&self) -> &'static str {
        "cloud"
    }

    fn get<'a>(&'a self, key: &'a str) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        let url = format!("{}/cache/lookup", self.base_url);
        let auth = format!("Bearer {}", self.auth_token);
        let body = serde_json::json!({"url": key}).to_string();
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

            Some(json.to_string())
        })
    }

    fn put(&self, _key: String, value: String, _cost: u8, _expires_at: u64) -> DeferredFuture {
        let url = format!("{}/cache/store", self.base_url);
        let auth = format!("Bearer {}", self.auth_token);
        let client = self.client.clone();
        Box::pin(async move {
            let _ = client
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .body(value)
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
pub async fn ping_cloud_token(token: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let url = format!("{DEFAULT_CLOUD_HOST}/cache/lookup");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token.trim()))
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
