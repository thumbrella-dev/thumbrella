//! Higher-tier handoff - the data bundle forwarded to tier-2/3 renderers.
//!
//! [`ThumbHandoff`] is the serialisable projection of the three portable
//! sub-structs on [`crate::cook::ThumbCook`] that travel between tiers.  It is
//! built by [`crate::cook::ThumbCook::take_handoff`] on the sending side and
//! consumed by [`crate::cook::ThumbCook::from_handoff`] on the receiving tier
//! to reconstruct the cook state at the render entry point.
//!
//! What travels and why:
//! - [`InputSpec`]      - original caller inputs; receiver needs url/etag.
//! - [`MediaInfo`]      - sniffed type info; skips re-running connect+inspect.
//! - [`SourceIdentity`] - cache key; tier-1 stores the result after receipt.
//! - `first_page`       - head-start bytes; receiver parses without a new request.
//!
//! What does NOT travel:
//! - `runtime`       - each tier constructs its own.
//! - `http_buf`      - live resource; moved via
//!   [`crate::cook::ThumbCook::http_take_reader`] on the in-process path,
//!   reconnected fresh on the out-of-process (serialised) path.
//! - `render_image`  - not yet populated at handoff time.
//! - `tel_*`         - per-tier; each tier tracks its own timing.
//! - `out_*`         - receiver populates fresh.
//!
//! # Custom handoff implementations
//!
//! [`post_handoff`] checks the internal `HANDOFF_IMPL` hook first.  Host
//! crates (e.g. a
//! Cloudflare Workers crate) call [`register_handoff_fn`] at startup to inject
//! a transport that fits their runtime (service bindings, `wasm_bindgen::Fetch`,
//! etc.).  If nothing is registered the function falls back to the native
//! reqwest implementation (when `feature = "native"`) or returns an error.
//!
//! # 503 scaleback
//!
//! A handoff target that answers `503 Service Unavailable` is retried with
//! exponential backoff plus equal jitter until [`HANDOFF_BUDGET_SECS`] elapses.
//! The healthy path pays nothing for this: the first attempt runs with no timer
//! and no deadline bookkeeping, and the budget clock starts only when a 503
//! actually arrives - a rare case.  From then on the budget covers every retry
//! delay plus each retried request.
//! When the budget expires, [`post_handoff`] returns an error for which
//! [`is_timeout_error`] is `true`; the cook pipeline turns that into a
//! placeholder result with message [`TIMEOUT_MESSAGE`].  Custom transports
//! signal a retryable 503 by returning [`unavailable_error`].

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::channel::oneshot;

use crate::cook::{InputSpec, MediaInfo, SourceIdentity};
use crate::result::{ThumbResult, ThumbTrace};
use serde::{Deserialize, Serialize};

/// Shared secret header name for server-to-server requests.
pub const HANDSHAKE_HEADER: &str = "x-tbr-handshake";

/// Retry budget for an overloaded handoff target, in seconds.
///
/// Measured from the moment the first `503 Service Unavailable` arrives.
/// Healthy handoffs build no timer at all, so this cost only applies while a
/// target is actually overloaded.  When the budget elapses the handoff
/// resolves to a placeholder result.
///
/// Kept at 15s because thumbnail responses have a hard 30s platform cap and
/// the measured worst-case tier-2 cold start (Modal) is ~8s: 8s to reach the
/// first 503 plus the full retry budget still lands under the cap with margin.
pub const HANDOFF_BUDGET_SECS: u64 = 15;

/// Message attached to the placeholder result produced when the budget elapses.
pub const TIMEOUT_MESSAGE: &str = "handoff timeout";

/// Prefix marking a retryable 503 ("service unavailable") failure.
const UNAVAILABLE_MARKER: &str = "handoff unavailable: ";

/// Prefix marking an elapsed handoff budget.
const TIMEOUT_MARKER: &str = "handoff timeout: ";

/// First retry delay in milliseconds.  Doubles per attempt up to [`RETRY_CAP_MS`].
const RETRY_BASE_MS: u64 = 250;

/// Upper bound for a single retry delay, before jitter is applied.
const RETRY_CAP_MS: u64 = 4_000;

/// Build the canonical retryable-503 error string.
///
/// A [`register_handoff_fn`] transport that receives `503 Service Unavailable`
/// should return this so [`post_handoff`] retries within the handoff budget
/// instead of failing immediately.
pub fn unavailable_error(detail: impl std::fmt::Display) -> String {
    format!("{UNAVAILABLE_MARKER}{detail}")
}

/// `true` when `error` was produced by [`unavailable_error`].
pub fn is_unavailable_error(error: &str) -> bool {
    error.starts_with(UNAVAILABLE_MARKER)
}

/// Build the error returned when the handoff budget elapses.
pub fn timeout_error(budget: Duration) -> String {
    format!("{TIMEOUT_MARKER}no response within {:.1}s", budget.as_secs_f64())
}

/// `true` when `error` means the handoff budget elapsed.
pub fn is_timeout_error(error: &str) -> bool {
    error.starts_with(TIMEOUT_MARKER)
}

/// Serialisable bundle forwarded to a higher-tier renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbHandoff {
    pub input: InputSpec,
    pub media: MediaInfo,
    pub src: SourceIdentity,

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

//  Injection hook

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
/// Call once at startup from the host crate - e.g. in the Cloudflare Workers
/// `fetch` setup, or in a native server that needs a different transport.
/// Subsequent calls are silently ignored (first writer wins, same as `OnceLock`).
pub fn register_handoff_fn(f: Box<HandoffFn>) {
    let _ = HANDOFF_IMPL.set(f);
}

//  post_handoff

/// Send a handoff payload to another tier and return both result + trace.
///
/// `headers` are additional HTTP headers to include in the handoff request
/// (e.g. `x-tbr-handshake`, `Authorization`, custom auth keys).
///
/// Dispatch priority:
/// 1. `HANDOFF_IMPL` - injected fn registered at startup (e.g. Workers).
/// 2. `native_post_handoff` - reqwest implementation for `feature = "native"`.
/// 3. Error - non-native build with no registered implementation.
///
/// The first attempt is untimed: a target that answers normally never touches
/// the retry machinery.  A `503 Service Unavailable` switches on a jittered
/// exponential scaleback bounded by [`HANDOFF_BUDGET_SECS`] (started at the
/// first 503); when it expires the returned error satisfies
/// [`is_timeout_error`].
pub async fn post_handoff(
    base_url: &str,
    headers: &HashMap<String, String>,
    payload: &ThumbHandoff,
) -> Result<HandoffResponse, String> {
    post_handoff_with_budget(
        base_url,
        headers,
        payload,
        Duration::from_secs(HANDOFF_BUDGET_SECS),
    )
    .await
}

/// [`post_handoff`] with an explicit budget - used by tests to keep them fast.
pub(crate) async fn post_handoff_with_budget(
    base_url: &str,
    headers: &HashMap<String, String>,
    payload: &ThumbHandoff,
    budget: Duration,
) -> Result<HandoffResponse, String> {
    retry_handoff(budget, || dispatch_handoff(base_url, headers, payload)).await
}

/// Select the active transport and start one handoff attempt.
fn dispatch_handoff(
    base_url: &str,
    headers: &HashMap<String, String>,
    payload: &ThumbHandoff,
) -> HandoffFut {
    if let Some(f) = HANDOFF_IMPL.get() {
        return f(base_url.to_string(), headers.clone(), payload.clone());
    }
    Box::pin(native_post_handoff(base_url.to_string(), headers.clone(), payload.clone()))
}

/// Run handoff attempts until one succeeds, a non-retryable error occurs, or
/// the retry budget elapses.
///
/// The healthy path is a single untimed `send().await` - no timer is created
/// and no clock is read.  Only a 503 (see [`unavailable_error`]) switches on
/// the scaleback: the budget clock starts at that first 503, retries sleep for
/// a jittered exponential delay, and each retried request is raced against the
/// remaining budget so a hung target cannot outlive it.
async fn retry_handoff<F>(budget: Duration, mut send: F) -> Result<HandoffResponse, String>
where
    F: FnMut() -> HandoffFut,
{
    match send().await {
        Ok(response) => return Ok(response),
        Err(error) if !is_unavailable_error(&error) => return Err(error),
        // 503: fall through to the scaleback path below.
        Err(_) => {}
    }

    let deadline = web_time::Instant::now() + budget;
    let mut attempt: u32 = 0;

    loop {
        attempt = attempt.saturating_add(1);
        let delay = Duration::from_millis(retry_delay_ms(attempt));
        let left = deadline.saturating_duration_since(web_time::Instant::now());
        if delay >= left {
            // The next delay would exhaust the budget: wait out the remainder
            // so the caller still sees a full budget's worth of trying.
            sleep_for(left).await;
            return Err(timeout_error(budget));
        }
        sleep_for(delay).await;

        let remaining = deadline.saturating_duration_since(web_time::Instant::now());
        if remaining.is_zero() {
            return Err(timeout_error(budget));
        }

        match with_budget(send(), remaining).await {
            Ok(response) => return Ok(response),
            Err(error) if is_timeout_error(&error) || !is_unavailable_error(&error) => {
                return Err(error);
            }
            // Still overloaded: scale back again.
            Err(_) => {}
        }
    }
}

/// Race a retried `request` against the remaining budget.
///
/// Only used after a 503 - the healthy first attempt is never wrapped, so no
/// timer is constructed on the common path.
async fn with_budget(request: HandoffFut, budget: Duration) -> Result<HandoffResponse, String> {
    use std::task::Poll;

    let mut request = request;
    let mut timer = Box::pin(sleep_for(budget));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(result) = request.as_mut().poll(cx) {
            return Poll::Ready(result);
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(timeout_error(budget)));
        }
        Poll::Pending
    })
    .await
}

/// Jittered exponential backoff for attempt `attempt` (1-based).
///
/// Equal jitter: half the ceiling is fixed, the other half is uniform in
/// `0..=half`, so the result lands in `ceiling/2..=ceiling`.  The ceiling
/// doubles per attempt from [`RETRY_BASE_MS`] up to [`RETRY_CAP_MS`].  The
/// floor keeps a flapping target from being hammered with near-zero delays.
fn retry_delay_ms(attempt: u32) -> u64 {
    let exponent = attempt.min(6);
    let ceiling = RETRY_BASE_MS.saturating_mul(1u64 << exponent).min(RETRY_CAP_MS);
    let half = ceiling / 2;
    let spread = ceiling - half;
    half + jitter(spread + 1)
}

/// Uniform random value in `0..ceiling_ms` from a process-local xorshift PRNG.
///
/// Seeded from the wall clock and advanced on every call.  Deliberately avoids
/// `getrandom` so the same code works on wasm32 without extra features.
fn jitter(ceiling_ms: u64) -> u64 {
    if ceiling_ms == 0 {
        return 0;
    }
    static STATE: AtomicU64 = AtomicU64::new(0);

    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        state = web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if state == 0 {
            state = 0x9E37_79B9_7F4A_7C15;
        }
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    STATE.store(state, Ordering::Relaxed);

    state.wrapping_mul(0x2545_F491_4F6C_DD1D) % ceiling_ms
}

/// Sleep for `delay` on the current runtime.
#[cfg(feature = "native")]
async fn sleep_for(delay: Duration) {
    tokio::time::sleep(delay).await;
}

/// Sleep for `delay` on a non-native (wasm32) runtime.
#[cfg(not(feature = "native"))]
async fn sleep_for(delay: Duration) {
    futures_timer::Delay::new(delay).await;
}

/// Reqwest-based handoff - used in native builds when no custom fn is registered.
#[cfg(feature = "native")]
async fn native_post_handoff(
    base_url: String,
    headers: HashMap<String, String>,
    payload: ThumbHandoff,
) -> Result<HandoffResponse, String> {
    let endpoint = format!("{}/handoff", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .tcp_nodelay(true)
        .build()
        .map_err(|e| format!("handoff client init failed: {e}"))?;

    let mut req = client.post(&endpoint).json(&payload);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await.map_err(|e| format!("handoff request failed: {e}"))?;
    let status = resp.status();

    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            return Err(unavailable_error(format!("handoff server returned {status}: {body}")));
        }
        return Err(format!("handoff server returned {status}: {body}"));
    }

    resp.json::<HandoffResponse>()
        .await
        .map_err(|e| format!("handoff response decode failed: {e}"))
}

/// Non-native stub - reached only when no implementation was registered.
#[cfg(not(feature = "native"))]
async fn native_post_handoff(
    _base_url: String,
    _headers: HashMap<String, String>,
    _payload: ThumbHandoff,
) -> Result<HandoffResponse, String> {
    Err("no handoff implementation registered; \
         call tier1::handoff::register_handoff_fn() at startup"
        .to_string())
}

//  HandoffInflight

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

impl Default for HandoffInflight {
    fn default() -> Self {
        Self::new()
    }
}

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

//  Tests

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn ok_response() -> HandoffResponse {
        HandoffResponse { result: ThumbResult::default(), trace: ThumbTrace::default() }
    }

    fn retryable(detail: &str) -> HandoffFut {
        let error = unavailable_error(detail);
        Box::pin(async move { Err(error) })
    }

    #[test]
    fn error_classification_uses_distinct_markers() {
        let unavailable = unavailable_error("handoff target returned HTTP 503");
        assert!(is_unavailable_error(&unavailable));
        assert!(!is_timeout_error(&unavailable));

        let timeout = timeout_error(Duration::from_secs(HANDOFF_BUDGET_SECS));
        assert!(is_timeout_error(&timeout));
        assert!(!is_unavailable_error(&timeout));

        assert!(!is_unavailable_error("connect refused"));
        assert!(!is_timeout_error("connect refused"));
    }

    #[test]
    fn retry_delays_stay_within_the_jittered_ceiling() {
        for attempt in 1..=20 {
            let ceiling = RETRY_BASE_MS
                .saturating_mul(1u64 << attempt.min(6))
                .min(RETRY_CAP_MS);
            for _ in 0..50 {
                let delay = retry_delay_ms(attempt);
                assert!(delay >= ceiling / 2, "delay {delay} below floor {ceiling}");
                assert!(delay <= ceiling, "delay {delay} above ceiling {ceiling}");
            }
        }
        assert_eq!(jitter(0), 0);
    }

    #[tokio::test]
    async fn success_on_first_attempt_does_not_retry() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let outcome = retry_handoff(Duration::from_secs(2), move || {
            counter.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(ok_response()) })
        })
        .await;

        assert!(outcome.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn non_retryable_errors_fail_immediately() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let outcome = retry_handoff(Duration::from_secs(2), move || {
            counter.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err("connection refused".to_string()) })
        })
        .await;

        assert_eq!(outcome.unwrap_err(), "connection refused");
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn retries_503_then_succeeds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let outcome = retry_handoff(Duration::from_secs(5), move || {
            let attempt = counter.fetch_add(1, Ordering::Relaxed);
            if attempt == 0 {
                retryable("handoff target returned HTTP 503")
            } else {
                Box::pin(async { Ok(ok_response()) })
            }
        })
        .await;

        assert!(outcome.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn persistent_503_times_out_at_the_budget() {
        // The first backoff ceiling is 500ms, so an 800ms budget guarantees
        // at least one retry before the scaleback exhausts the deadline.
        let budget = Duration::from_millis(800);
        let started = web_time::Instant::now();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let error = retry_handoff(budget, move || {
            counter.fetch_add(1, Ordering::Relaxed);
            retryable("handoff target returned HTTP 503")
        })
        .await
        .unwrap_err();

        assert!(is_timeout_error(&error), "expected timeout, got: {error}");
        assert!(attempts.load(Ordering::Relaxed) >= 2);
        assert!(started.elapsed() >= budget);
        assert!(started.elapsed() < budget + Duration::from_secs(1));
    }

    #[tokio::test]
    async fn first_attempt_is_not_bounded_by_the_budget() {
        // The healthy path runs before any timer exists, so even a zero budget
        // must not cut off (or fail) the initial request.
        let outcome = retry_handoff(Duration::ZERO, || {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(ok_response())
            })
        })
        .await;

        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn hung_retry_times_out_at_the_budget() {
        // First attempt answers 503 immediately; every retry then hangs.  The
        // first jittered delay is at most 500ms, so an 800ms budget guarantees
        // the hung retry is reached before the deadline.
        let budget = Duration::from_millis(800);
        let started = web_time::Instant::now();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let error = retry_handoff(budget, move || {
            if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                retryable("handoff target returned HTTP 503")
            } else {
                Box::pin(std::future::pending::<Result<HandoffResponse, String>>())
            }
        })
        .await
        .unwrap_err();

        assert!(is_timeout_error(&error), "expected timeout, got: {error}");
        assert!(attempts.load(Ordering::Relaxed) >= 2);
        assert!(started.elapsed() >= budget);
        assert!(started.elapsed() < budget + Duration::from_secs(1));
    }
}
