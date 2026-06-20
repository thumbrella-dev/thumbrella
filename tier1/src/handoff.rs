//! Higher-tier handoff — the data bundle forwarded to tier-2/3 renderers.
//!
//! [`ThumbHandoff`] is the serialisable projection of the three portable
//! sub-structs on [`ThumbCook`] that travel between tiers.  It is built by
//! [`ThumbCook::to_handoff`] on the sending side and consumed by
//! `ThumbCook::from_handoff` on the receiving tier to reconstruct the cook
//! state at the render entry point.
//!
//! What travels and why:
//! - [`InputSpec`]      — original caller inputs; receiver needs url/etag.
//! - [`MediaInfo`]      — sniffed type info; skips re-running connect+inspect.
//! - [`SourceIdentity`] — cache key; tier-1 stores the result after receipt.
//! - `first_page`       — head-start bytes; receiver parses without a new request.
//!
//! What does NOT travel:
//! - `runtime`       — each tier constructs its own.
//! - `http_buf`      — live resource; moved via [`ThumbCook::http_take_reader`]
//!                     on the in-process path, reconnected fresh on the
//!                     out-of-process (serialised) path.
//! - `render_image`  — not yet populated at handoff time.
//! - `tel_*`         — per-tier; each tier tracks its own timing.
//! - `out_*`         — receiver populates fresh.
//!
//! # Custom handoff implementations
//!
//! [`post_handoff`] checks [`HANDOFF_IMPL`] first.  Host crates (e.g. a
//! Cloudflare Workers crate) call [`register_handoff_fn`] at startup to inject
//! a transport that fits their runtime (service bindings, `wasm_bindgen::Fetch`,
//! etc.).  If nothing is registered the function falls back to the native
//! reqwest implementation (when `feature = "native"`) or returns an error.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use futures::channel::oneshot;

use serde::{Deserialize, Serialize};
use crate::cook::{InputSpec, MediaInfo, SourceIdentity};
use crate::result::{ThumbResult, ThumbTrace};

/// Shared secret header name for server-to-server requests.
pub const HANDSHAKE_HEADER: &str = "x-tbr-handshake";

/// Serialisable bundle forwarded to a higher-tier renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbHandoff {
    pub input:  InputSpec,
    pub media:  MediaInfo,
    pub src:    SourceIdentity,

    /// First ~4 KiB of the remote file captured from the inspect page cache.
    ///
    /// Forwarded as a head start on header parsing.  `None` when no data was
    /// cached before the connection was closed (unusual).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_page: Option<Vec<u8>>,
}

/// HTTP payload returned by `/handoff`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffResponse {
    pub result: ThumbResult,
    pub trace: ThumbTrace,
}

// ── Injection hook ────────────────────────────────────────────────────────────

/// Boxed future returned by an injected handoff function.
///
/// `+ 'static` is required so the future can be `Box`-pinned without a
/// lifetime parameter.  All captured values must be owned.
pub type HandoffFut = Pin<Box<dyn Future<Output = Result<HandoffResponse, String>> + Send>>;

/// Injected handoff function signature.
///
/// Receives the handoff target base URL (`<url>/handoff` is the endpoint),
/// an optional map of HTTP headers, and the serialised `ThumbHandoff` payload.
/// Returns a future that resolves to a `HandoffResponse` or an error string.
pub type HandoffFn = dyn Fn(String, HashMap<String, String>, ThumbHandoff) -> HandoffFut + Send + Sync;

/// Process-global injected handoff implementation.
///
/// Set once via [`register_handoff_fn`].  When present, [`post_handoff`]
/// delegates entirely to it and the native reqwest path is bypassed.
static HANDOFF_IMPL: OnceLock<Box<HandoffFn>> = OnceLock::new();

/// Register a custom handoff transport for the current process.
///
/// Call once at startup from the host crate — e.g. in the Cloudflare Workers
/// `fetch` setup, or in a native server that needs a different transport.
/// Subsequent calls are silently ignored (first writer wins, same as `OnceLock`).
pub fn register_handoff_fn(f: Box<HandoffFn>) {
    let _ = HANDOFF_IMPL.set(f);
}

// ── post_handoff ──────────────────────────────────────────────────────────────

/// Send a handoff payload to another tier and return both result + trace.
///
/// `headers` are additional HTTP headers to include in the handoff request
/// (e.g. `x-tbr-handshake`, `Authorization`, custom auth keys).
///
/// Dispatch priority:
/// 1. [`HANDOFF_IMPL`] — injected fn registered at startup (e.g. Workers).
/// 2. `native_post_handoff` — reqwest implementation for `feature = "native"`.
/// 3. Error — non-native build with no registered implementation.
pub async fn post_handoff(
    base_url: &str,
    headers: &HashMap<String, String>,
    payload: &ThumbHandoff,
) -> Result<HandoffResponse, String> {
    if let Some(f) = HANDOFF_IMPL.get() {
        return f(
            base_url.to_string(),
            headers.clone(),
            payload.clone(),
        )
        .await;
    }
    native_post_handoff(base_url, headers, payload).await
}

/// Reqwest-based handoff — used in native builds when no custom fn is registered.
#[cfg(feature = "native")]
async fn native_post_handoff(
    base_url: &str,
    headers: &HashMap<String, String>,
    payload: &ThumbHandoff,
) -> Result<HandoffResponse, String> {
    let endpoint = format!("{}/handoff", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .http2_adaptive_window(true)
        .tcp_nodelay(true)
        .build()
        .map_err(|e| format!("handoff client init failed: {e}"))?;

    let mut req = client.post(&endpoint).json(payload);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("handoff request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("handoff server returned {status}: {body}"));
    }

    resp.json::<HandoffResponse>()
        .await
        .map_err(|e| format!("handoff response decode failed: {e}"))
}

/// Non-native stub — reached only when no implementation was registered.
#[cfg(not(feature = "native"))]
async fn native_post_handoff(
    _base_url: &str,
    _headers: &HashMap<String, String>,
    _payload: &ThumbHandoff,
) -> Result<HandoffResponse, String> {
    Err(
        "no handoff implementation registered; \
         call tier1::handoff::register_handoff_fn() at startup"
            .to_string(),
    )
}

// ── HandoffInflight ───────────────────────────────────────────────────────────

struct InflightSlot {
    waiters: Vec<oneshot::Sender<Arc<Result<HandoffResponse, String>>>>,
}

/// Deduplicates concurrent handoffs that resolve to the same cache key.
///
/// When multiple requests arrive for the same content while a handoff is
/// already in flight, joiners suspend (yield at the oneshot `.await`) until
/// the leader's handoff resolves, then share its result.
///
/// # Threading model
///
/// Uses `parking_lot::Mutex` (never held across an `.await`) and
/// `futures::channel::oneshot` (no-std compatible).  Works on both
/// multi-threaded native tokio and single-threaded Cloudflare Workers.
#[derive(Clone)]
pub struct HandoffInflight(Arc<parking_lot::Mutex<HashMap<String, InflightSlot>>>);

impl HandoffInflight {
    pub fn new() -> Self {
        Self(Arc::new(parking_lot::Mutex::new(HashMap::new())))
    }

    /// Attempt to register as the leader for `key`.
    ///
    /// Returns `None`     → caller is the **leader**; perform the handoff,
    ///                       then call [`complete`](Self::complete).
    /// Returns `Some(rx)` → caller is a **joiner**; `.await rx` to receive
    ///                       the shared result once the leader finishes.
    pub fn try_lead(
        &self,
        key: &str,
    ) -> Option<oneshot::Receiver<Arc<Result<HandoffResponse, String>>>> {
        let mut map = self.0.lock();
        if map.contains_key(key) {
            let (tx, rx) = oneshot::channel();
            map.get_mut(key).unwrap().waiters.push(tx);
            Some(rx)
        } else {
            map.insert(key.to_string(), InflightSlot { waiters: vec![] });
            None
        }
    }

    /// Notify all joiners waiting on `key` with the resolved result.
    ///
    /// Must be called by the leader after the handoff resolves (success or
    /// error).  Joiners whose receiver was already dropped are silently skipped.
    pub fn complete(&self, key: &str, result: Arc<Result<HandoffResponse, String>>) {
        let slot = self.0.lock().remove(key);
        if let Some(slot) = slot {
            for tx in slot.waiters {
                let _ = tx.send(Arc::clone(&result));
            }
        }
    }
}
