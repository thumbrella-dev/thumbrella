//! Trace-log integration — `TraceBackend` trait, `TraceStore` runtime holder,
//! and DSN-based backend construction.
//!
//! ## Async contract
//!
//! `record_task` returns an owned [`DeferredFuture`] the caller schedules via
//! [`AfterResponse`] — writes never block the response path.
//!
//! ## One file per tier
//!
//! Each tier writes to its own file.  Correlation across tiers is done via
//! `session_id` on [`ThumbTrace`].  No inter-process locking is required.
//!
//! ## Workers Analytics Engine
//!
//! The WAE backend lives in the downstream workers crate (same pattern as
//! `FetchStream` not living here).  This crate only defines the trait.

use std::sync::Arc;

use crate::after::{AfterResponse, DeferredFuture};
use crate::result::ThumbTrace;

#[cfg(feature = "native")]
pub mod ndjson;

// ── Backend trait ─────────────────────────────────────────────────────────────

/// A single trace/log storage backend.  Implementations must be `Send + Sync`.
pub trait TraceBackend: Send + Sync {
    /// Human-readable name used in diagnostics (e.g. `"ndjson"`).
    fn name(&self) -> &'static str;

    /// Return a `'static + Send` future that appends this trace record.
    ///
    /// The future is owned and pushed onto [`AfterResponse`] so the write
    /// runs after the HTTP response is sent.  Errors must be swallowed inside.
    fn record_task(&self, trace: Arc<ThumbTrace>) -> DeferredFuture;
}

// ── TraceStore ────────────────────────────────────────────────────────────────

/// Holds the active trace backends for the process.
///
/// Cheap to clone — backends are behind `Arc`.  An empty store
/// (`TraceStore::none()`) is used when no sink is configured.
#[derive(Clone, Default)]
pub struct TraceStore {
    backends: Vec<Arc<dyn TraceBackend>>,
}

impl TraceStore {
    /// Construct a store from a list of backends.
    pub fn new(backends: Vec<Arc<dyn TraceBackend>>) -> Self {
        Self { backends }
    }

    /// Empty store — no writes.
    pub fn none() -> Self {
        Self { backends: Vec::new() }
    }

    /// Schedule a trace record into all backends via `after`.
    ///
    /// The write runs after the HTTP response via [`AfterResponse::drain_spawn`]
    /// (native) or `ctx.wait_until` (Workers).  Nothing happens when the store
    /// is empty.
    pub fn record(&self, trace: ThumbTrace, after: &mut AfterResponse) {
        if self.backends.is_empty() { return; }
        let trace = Arc::new(trace);
        for backend in &self.backends {
            after.push(backend.record_task(Arc::clone(&trace)));
        }
    }
}

// ── DSN parser ────────────────────────────────────────────────────────────────

/// Build a backend list from a `TBR_TRACE` DSN string.
///
/// Supported schemes:
/// - `ndjson:<path>` — Append-only NDJSON file (path may be absolute or relative)
///
/// Returns an error string if the scheme is unknown or the backend fails to open.
#[cfg(feature = "native")]
pub fn open_from_dsn(dsn: &str) -> Result<Vec<Arc<dyn TraceBackend>>, String> {
    if let Some(path) = dsn.strip_prefix("ndjson:") {
        let backend = ndjson::NdjsonTraceBackend::open(path)
            .map_err(|e| format!("ndjson trace: {e}"))?;
        return Ok(vec![Arc::new(backend)]);
    }
    Err(format!("unsupported trace DSN scheme: {dsn}"))
}
