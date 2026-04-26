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

use image::DynamicImage;

use crate::http_buf::{HttpBuffer, HttpStream};
use crate::result::{JobStatus, RenderHandler, ThumbResult};
use crate::pipeline;

// ── CallerContext ─────────────────────────────────────────────────────────────

/// How this cook was invoked — stored in [`ThumbTrace`], never sent to clients.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CallerContext {
    /// HTTP client identified by IP address.
    ///
    /// The IP is taken from the connection or from a trusted proxy header
    /// (configured via `AppConfig`).  When proxy trust is disabled the
    /// forwarded header is ignored and the raw connection IP is used.
    Ip { addr: String },
    /// Invoked from the `tier1 thumb` CLI subcommand.
    Cli,
    /// Invoked programmatically as a library (e.g. unit tests, embedders).
    Library,
}


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
    /// SHA-256(customer_id ":" content_identity) — used as the storage key.
    ///
    /// `content_identity` is the best available stable identifier for the
    /// content: a server-provided hash header when present, otherwise the
    /// canonical URL.  Always base64url-encoded (no padding) for brevity.
    pub cache_hash: Option<String>,
    /// Which header (or fallback) was used as the identity input for `cache_hash`.
    /// Examples: `"x-amz-checksum-sha256"`, `"etag"`, `"url"`.
    pub cache_hash_source: Option<String>,
    /// Upstream freshness token (ETag or Last-Modified) in the opaque format
    /// produced by `etag_from_headers`.  Returned to the client as `etag` in
    /// the `ThumbResult` so they can send it back on future requests.
    pub source_etag: Option<String>,

    // ── Download metrics ──────────────────────────────────────────────────────
    /// Bytes received from the primary forward stream.
    pub download_bytes: u64,
    /// Extra bytes from a tail Range request (e.g. TIFF IFD ZIP).
    pub download_tail_bytes: u64,
    /// Seconds to establish the HTTP connection (TCP + response headers).
    pub connect_secs: f64,

    // ── Render metrics ────────────────────────────────────────────────────────
    /// Seconds spent in the inspect step (sniff + type detection).
    pub inspect_secs: f64,
    /// Seconds spent in the shortcut step (EXIF scan, progressive read, ZIP tail).
    pub shortcut_secs: f64,
    /// Seconds spent in the render step (decode + colour convert).
    pub render_secs: f64,
    /// Seconds spent in the deliver step (resize + mozjpeg encode).
    pub deliver_secs: f64,
    /// Pixel dimensions of the image buffer that entered the render/encode step.
    /// Stored as `[width, height]`.  For the full-decode path this equals the
    /// original media dimensions; for shortcut paths it is the embedded
    /// thumbnail's decoded size.
    pub render_resolution: Option<[u32; 2]>,
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
    // ── Cache ─────────────────────────────────────────────────────────────────────
    /// Which cache backend produced this result, if any.
    /// `None` until the cache layer is wired up.
    pub cache_hit: Option<crate::result::CacheOutcome>,

    // ── Render path ───────────────────────────────────────────────────────────────
    /// How the thumbnail was ultimately produced.
    pub render_handler: RenderHandler,

    // ── Request context ───────────────────────────────────────────────────────────
    /// How (and from where) this cook was invoked.
    /// Set by the entry point; absent for programmatic calls that don’t specify.
    pub caller: Option<CallerContext>,
    /// True if the client connection dropped before this item completed.
    /// Items that finished before the drop keep `cancelled = false`.
    pub cancelled: bool,
    /// Server identifier — a 3-letter Cloudflare colo code (e.g. `"SJC"`),
    /// or an operator-configured name for self-hosted deployments.
    /// Set by the entry point from the `TBR_SERVER` environment variable.
    pub server: Option<String>,
    /// Thumbrella build version that processed this item.
    pub version: String,}

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
    /// Decoded pixel buffer; populated by shortcut, consumed and cleared by deliver.
    pub render: Option<DynamicImage>,
}

impl<S: HttpStream> ThumbCook<S> {
    /// Create a new cook from a spec.
    pub fn new(spec: ThumbSpec) -> Self {
        let url = spec.url.clone();
        Self {
            spec,
            response: ThumbResult { url, ..ThumbResult::default() },
            trace: ThumbTrace {
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..ThumbTrace::default()
            },
            cancel: Arc::new(AtomicBool::new(false)),
            http: None,
            render: None,
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

    /// Snapshot `http.bytes_fetched()` into `trace.download_bytes` and
    /// `response.download_size`, unless those fields have already been set by
    /// a pipeline step (shortcut success paths set them directly).
    ///
    /// Call this at every pipeline exit point so the trace always reflects the
    /// real bytes pulled from the network, even on defer or failure paths.
    pub fn stamp_download_bytes(&mut self) {
        let fetched = self.http.as_ref().map(|h| h.bytes_fetched()).unwrap_or(0);
        // Only overwrite if the step hasn't set a more precise value already.
        if self.trace.download_bytes == 0 && fetched > 0 {
            self.trace.download_bytes = fetched;
        }
        if self.response.download_size == 0 && fetched > 0 {
            self.response.download_size = fetched;
        }
    }

    /// Consume the cook and return the finished response and trace.
    pub fn into_result(self) -> (ThumbResult, ThumbTrace) {
        (self.response, self.trace)
    }

    /// Run the full pipeline and return `(result, trace)`.
    ///
    /// Sequences: connect → inspect → shortcut → [tier-gate] → deliver.
    /// Each step is called only if the previous one left `cook.http` open
    /// (`Some`).  Steps signal completion by setting `cook.http = None`.
    pub async fn run(mut self) -> (ThumbResult, ThumbTrace) {
        let t0 = std::time::Instant::now();

        let t_dl = std::time::Instant::now();
        pipeline::connect(&mut self).await;
        self.trace.connect_secs = t_dl.elapsed().as_secs_f64();
        if self.http.is_none() { self.stamp_download_bytes(); self.response.duration = t0.elapsed().as_secs_f64(); return self.into_result(); }

        // Derive the cache storage key: SHA-256( customer_id ":" content_identity ).
        // Prefer a server-supplied content hash (ETag, Content-MD5, AWS/GCS checksum
        // headers) — these are stable even when URLs contain signing tokens.
        // Falls back to the canonical URL when no usable hash header is present.
        // Always base64url-encoded (no padding) for compact, URL-safe keys.
        {
            use sha2::{Sha256, Digest};
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            use crate::source::content_hash_from_headers;

            let (identity, source) = self.http
                .as_ref()
                .and_then(|h| content_hash_from_headers(&h.headers))
                .map(|(hash, src)| (hash, src.to_string()))
                .unwrap_or_else(|| {
                    let url = self.trace.final_url.as_deref()
                        .or(self.trace.canonical_url.as_deref())
                        .unwrap_or(self.spec.url.as_str());
                    (url.to_string(), "url".to_string())
                });

            let input = format!(
                "{}:{identity}",
                self.trace.customer_id.as_deref().unwrap_or("")
            );
            self.trace.cache_hash = Some(URL_SAFE_NO_PAD.encode(Sha256::digest(input.as_bytes())));
            self.trace.cache_hash_source = Some(source);
        }

        let t_inspect = std::time::Instant::now();
        pipeline::inspect(&mut self).await;
        self.trace.inspect_secs = t_inspect.elapsed().as_secs_f64();
        if self.http.is_none() { self.stamp_download_bytes(); self.response.duration = t0.elapsed().as_secs_f64(); return self.into_result(); }

        // Shortcut: try to populate cook.render cheaply (small file read, EXIF
        // thumbnail, ZIP preview, …).  Closes cook.http when done either way.
        let t_shortcut = std::time::Instant::now();
        pipeline::shortcut(&mut self).await;
        self.trace.shortcut_secs = t_shortcut.elapsed().as_secs_f64();
        self.stamp_download_bytes();

        if self.response.status == JobStatus::Success {
            // Shortcut fully produced the thumbnail — return without going through deliver.
            self.response.duration = t0.elapsed().as_secs_f64();
            return self.into_result();
        }

        if self.render.is_some() {
            // Shortcut populated the render buffer — deliver encodes the final JPEG.
            pipeline::deliver(&mut self).await;
            self.response.duration = t0.elapsed().as_secs_f64();
            return self.into_result();
        }

        // Shortcut found nothing — ensure the connection is closed.
        if let Some(h) = self.http.as_mut() { h.close().await; }
        self.http = None;

        // TODO: check kind/tier and handoff to tier 2/3.
        // TODO: fall back to placeholder icon.
        self.response.status = JobStatus::DeferServer;
        self.response.message =
            "deferred to higher-tier renderer (not yet connected)".to_string();
        self.response.duration = t0.elapsed().as_secs_f64();
        self.into_result()
    }
}
