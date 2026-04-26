//! Cook execution context — `ThumbSpec`, `ThumbCook`, `ThumbTrace`.
//!
//! # Concepts
//!
//! - [`ThumbSpec`] — a lightweight, portable description of work to do.
//!   No runtime state; cheap to clone; source-agnostic.  Can be constructed
//!   from an HTTP request, a tier handoff payload, a dev tool call, or a test.
//!
//! - [`ThumbCook`] — the execution context for one thumbnail.  Owns the spec,
//!   the accumulating [`ThumbResult`], internal telemetry ([`ThumbTrace`]),
//!   and the live resources that are allocated and dropped as pipeline steps
//!   run.  Consumed by [`ThumbCook::cook`], which returns a [`ThumbResult`].
//!
//! - [`ThumbTrace`] — internal per-item telemetry.  Never sent to clients.
//!   Populated incrementally as steps complete; written to the log sink when
//!   `cook()` finishes.
//!
//! - [`ThumbResult`] — the per-item result type.  Defined in `result`, used
//!   here as the cook output so there is no translation step at completion.
//!
//! # Cancellation
//!
//! `ThumbCook` holds a cancel flag (`Arc<AtomicBool>`) checked between pipeline
//! steps.  It is not yet exposed publicly — the primary cancellation mechanism
//! today is drop-based (dropping the future mid-await stops execution cleanly).
//! The flag is reserved for cooperative teardown: closing the upstream HTTP
//! connection gracefully when a client disconnects.  A public `ThumbCancelHandle`
//! will be added when we wire up the Cloudflare Workers `request.signal` callback
//! and the Axum disconnect path.
//!
//! # Build note
//!
//! This module must compile on `wasm32-unknown-unknown`.  No `std`-only types
//! except those already in scope via the `alloc` feature.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::http_buf::{HttpBuffer, HttpStream};
use crate::result::{JobStatus, ThumbResult};
use crate::pipeline;


// ── ThumbSpec ─────────────────────────────────────────────────────────────────

/// Portable, lightweight description of one thumbnail to produce.
///
/// Plain data with no runtime state; cheap to clone.  Can be built from any
/// entry point — HTTP request, tier handoff, developer tool — without coupling
/// the execution context to any particular transport.
#[derive(Debug, Clone, Default)]
pub struct ThumbSpec {
    /// Source URL to fetch and thumbnail.
    pub url: String,
    /// Previously returned `etag` from a `ThumbResult`.
    ///
    /// When supplied, the service issues a conditional fetch.  If the source
    /// is unchanged the result has `status: not_modified` and an empty
    /// `thumbnail` — the only case where thumbnail bytes are absent.
    ///
    /// Opaque prefix determines the upstream request header:
    /// - `E…` → `If-None-Match`
    /// - `M…` → `If-Modified-Since`
    pub etag: Option<String>,
    /// Allow `file://` URLs to be fetched from the local filesystem.
    ///
    /// **Security**: defaults to `false`.  Only the CLI entry point sets this
    /// to `true`; HTTP route handlers must never set it.  The pipeline
    /// `connect` step enforces this as a second line of defence.
    pub allow_local: bool,
}

impl ThumbSpec {
    /// Convenience constructor: spec for the given URL with all else defaulted.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Self::default() }
    }
}

// ── Live resource placeholders ────────────────────────────────────────────────

/// Decoded pixel buffer from the render step.
///
/// Stub — will wrap an `image::DynamicImage` or similar once rendering is live.
pub struct RenderImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Encoded JPEG ready for delivery.
///
/// Stub — will be produced by the deliver step (mozjpeg encode).
pub struct ThumbnailImage {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

// ── ThumbTrace ────────────────────────────────────────────────────────────────

/// Internal per-item telemetry — the server's private record of work done.
///
/// Never sent to clients.  Populated incrementally as pipeline steps run;
/// emitted to the log sink when the cook finishes.
#[derive(Debug, Default, serde::Serialize)]
pub struct ThumbTrace {
    // ── Source identity ───────────────────────────────────────────────────────
    /// Canonical URL (query params / signing tokens stripped).
    pub canonical_url: Option<String>,
    /// Final URL after following HTTP redirects (if different from canonical).
    pub final_url: Option<String>,
    /// SHA-256 of the canonical URL — used as the storage key.
    pub url_hash: Option<String>,
    /// Upstream freshness token (ETag or Last-Modified) in the opaque format
    /// produced by `etag_from_headers`.  Returned to the client as `etag` in
    /// the `ThumbResult` so they can send it back on future requests.
    pub source_etag: Option<String>,

    // ── Download metrics ──────────────────────────────────────────────────────
    /// Bytes received from the primary forward stream.
    pub download_bytes: u64,
    /// Extra bytes from a tail Range request (e.g. TIFF IFD).
    pub download_tail_bytes: u64,
    /// Seconds waiting for upstream download(s).
    pub download_secs: f64,

    // ── Render metrics ────────────────────────────────────────────────────────
    /// Seconds spent in the render step (decode + colour convert).
    pub render_secs: f64,
    /// Seconds spent in the deliver step (resize + mozjpeg encode).
    pub encode_secs: f64,
    /// Pixel dimensions of the image buffer entering the encode step.
    pub encode_width: Option<u32>,
    pub encode_height: Option<u32>,
    /// Byte length of the encoded JPEG.
    pub thumbnail_bytes: Option<u64>,

    // ── Job provenance ────────────────────────────────────────────────────────
    /// Processing tier that produced the thumbnail (1 = tier1, 2 = tier2, …).
    pub job_tier: u8,
    /// Low-level renderer used (e.g. `"image_crate"`, `"libav"`, `"resvg"`).
    pub job_renderer: Option<String>,
    /// Codec or container detail (e.g. `"h264"`, `"deflate"`).
    pub job_codec: Option<String>,
    /// Seek offset used for video frame selection, in seconds.
    pub video_seek_secs: Option<f64>,

    // ── Tracing / attribution ───────────────────────────────────────────────────
    /// Groups multiple `ThumbTrace` records from the same inbound batch.
    /// Set by the entry point; absent for single-item or programmatic calls.
    pub session_id: Option<String>,
    /// Customer identifier for billing and quota attribution.
    /// Set by the entry point; absent for open/unauthenticated deployments.
    pub customer_id: Option<String>,
}

// ── ThumbCook ─────────────────────────────────────────────────────────────────

/// Execution context for one thumbnail.
///
/// `ThumbCook` is generic over the HTTP backend `S` so the same pipeline code
/// runs on native (reqwest) and Cloudflare Workers (workers-rs fetch) without
/// modification.  On the native side, use the `PlatformStream` type alias from
/// `http_buf` to avoid spelling out the generic everywhere.
///
/// Construct with [`ThumbCook::new`], then call [`ThumbCook::cook`] to run the
/// pipeline and get back a [`ThumbResult`].
pub struct ThumbCook<S: HttpStream> {
    /// The work description this cook was built from.
    pub spec: ThumbSpec,
    /// Accumulating result — mutated by each pipeline step.
    /// This is what gets cached and sent to the client when cooking finishes.
    pub response: ThumbResult,
    /// Internal telemetry — mutated by each step, never sent to clients.
    pub trace: ThumbTrace,

    // Cancel flag — checked between steps for cooperative teardown.
    // Not yet public; see module-level docs on cancellation.
    cancel: Arc<AtomicBool>,

    // Live resources — allocated and dropped as steps complete.
    /// Open HTTP connection; present from connect through render, then closed.
    pub http: Option<HttpBuffer<S>>,
    /// Decoded pixel buffer; present from render through deliver, then dropped.
    pub render: Option<RenderImage>,
    /// Encoded JPEG; present after deliver until `into_result()` is called.
    pub thumb: Option<ThumbnailImage>,
}

impl<S: HttpStream> ThumbCook<S> {
    /// Create a new cook from a spec.
    pub fn new(spec: ThumbSpec) -> Self {
        let url = spec.url.clone();
        Self {
            spec,
            response: ThumbResult { url, ..ThumbResult::default() },
            trace: ThumbTrace::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            http: None,
            render: None,
            thumb: None,
        }
    }

    /// Returns `true` if cancellation has been requested.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Mark the response as failed with a message.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.response.status = JobStatus::Failed;
        self.response.message = message.into();
    }

    /// Consume the cook and return the finished response and trace.
    pub fn into_result(self) -> (ThumbResult, ThumbTrace) {
        (self.response, self.trace)
    }

    /// Run the full pipeline and return `(result, trace)`.
    ///
    /// Sequences: preflight → connect → inspect → fallback/shortcut/render → deliver.
    /// Step functions live in `pipeline/` and are called from here as they are
    /// implemented.  Returns early if `cook.http` is `None` after a step
    /// (the step's signal that it set a definitive outcome).
    pub async fn run(mut self) -> (ThumbResult, ThumbTrace) {
        pipeline::connect(&mut self).await;
        if self.http.is_none() {
            // connect set a definitive outcome (error, 304, 4xx, 5xx) — done.
            return self.into_result();
        }

        pipeline::inspect(&mut self).await;
        if self.http.is_none() {
            return self.into_result();
        }

        // TODO: pipeline::render(&mut self).await;
        // TODO: pipeline::deliver(&mut self).await;

        // Pipeline incomplete — return what we have so far.
        // The status remains `Failed` until deliver sets it to `Success`.
        if self.response.message.is_empty() {
            self.response.message = "pipeline incomplete".to_string();
        }
        self.into_result()
    }
}
