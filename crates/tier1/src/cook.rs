//! Cook execution context — the central processing state for one thumbnail.
//!
//! # Design
//!
//! [`ThumbCook`] is a flat buffer of all state needed to process one thumbnail,
//! from initial inputs through to final output.  Pipeline steps read and write
//! fields directly — there are no intermediate aggregation types or synchronisation
//! bridges.
//!
//! Fields are grouped by prefix:
//!
//! | Prefix   | Contents |
//! |----------|----------|
//! | `in_`    | Caller inputs (set at construction, never mutated by pipeline) |
//! | `http_`  | HTTP connection metadata captured during `connect` |
//! | `media_` | Sniffed type info populated during `inspect` |
//! | `src_`   | Source identity — cache key, canonical URL, etag |
//! | `render_`| Pixel work state — decoded buffer, resolution, codec info |
//! | `out_`   | Client-facing output fields written as steps complete |
//! | `tel_`   | Per-step timing telemetry |
//! | `ctx_`   | Attribution / request context (caller, session, customer) |
//!
//! # Output views
//!
//! Neither [`ThumbResult`] nor [`ThumbTrace`] exist during processing.  They
//! are materialised only at two points:
//! - [`ThumbCook::to_result`] — called once to cache and return to the client.
//! - [`ThumbCook::to_trace`] — called once to emit to the configured log sink.
//!
//! # Tier handoff
//!
//! [`ThumbCook::to_handoff`] projects the three portable sub-structs
//! ([`InputSpec`], [`MediaInfo`], [`SourceIdentity`]) into a [`ThumbHandoff`]
//! for serialisation and forwarding to a higher-tier renderer.
//! [`ThumbCook::from_handoff`] reconstructs the cook on the receiving tier,
//! populating those same fields and setting `status = Processing` so the
//! pipeline enters at the render step.
//!
//! # Pipeline gate
//!
//! Steps check `cook.status == CookStatus::Processing` to decide whether to
//! continue.  Any step that encounters a terminal condition sets `cook.status`
//! to the appropriate [`CookStatus`] variant and returns — no other sentinel
//! checking is needed.
//!
//! # Build note
//!
//! This module must compile on `wasm32-unknown-unknown`.  No `std`-only types
//! except those already in scope via the `alloc` feature.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use image::{DynamicImage, RgbImage};
use serde_json::Value;

use crate::after::AfterResponse;
use crate::cache::CacheStore;
use crate::http_buf::{HttpBuffer, HttpStream};
use crate::media::{FileKind, Strategy};
use crate::result::{CacheOutcome, JobStatus, RenderHandler, ThumbResult, ThumbTrace};
use crate::spec::ShortcutLimits;
use crate::tracelog::TraceStore;
use crate::pipeline;
#[cfg(feature = "native")]
use crate::renderer::{RenderCook, SharedRenderer};

// ── CookStatus ────────────────────────────────────────────────────────────────

/// Internal pipeline gate.  Steps check `cook.status == CookStatus::Processing`
/// to decide whether to continue.  Any step that hits a terminal condition sets
/// this and returns immediately; no other sentinel is needed.
///
/// Maps to [`JobStatus`] via [`ThumbCook::to_result`] for the client view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CookStatus {
    /// Pipeline is running — the only "keep going" state.
    #[default]
    Processing,
    /// Thumbnail produced successfully.
    Complete,
    /// Conditional request; source unchanged since caller's supplied validator.
    NotModified,
    /// Terminal failure — message is in `out_message`.
    Failed,
    /// Request deferred — caller is rate-limited or over quota.
    DeferUser,
    /// No higher-tier renderer is configured or available.
    Unavailable,
    /// Handed off to a higher-tier renderer; streaming result pending.
    Rendering,
}

impl CookStatus {
    /// Whether the pipeline should continue.
    pub fn is_processing(self) -> bool {
        self == Self::Processing
    }
}

// ── CallerContext ─────────────────────────────────────────────────────────────

/// How this cook was invoked — written to [`ThumbTrace`], never sent to clients.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CallerContext {
    /// HTTP client identified by IP address.
    Ip { addr: String },
    /// Invoked from the `tier1 thumb` CLI subcommand.
    Cli,
    /// Invoked programmatically as a library (e.g. unit tests, embedders).
    Library,
}

// ── Runtime ───────────────────────────────────────────────────────────────────

/// Shared server-wide configuration.  One instance per process, referenced by
/// every concurrent cook via `Arc<Runtime>`.
///
/// On `wasm32-unknown-unknown` (Cloudflare Workers) `Arc` is zero-cost — the
/// atomic refcount compiles to plain loads/stores on a single-threaded isolate.
#[derive(Clone)]
pub struct Runtime {
    /// Active cache backends.  Empty when no cache is configured.
    pub cache: CacheStore,
    /// Active trace/log backends.  Empty when no sink is configured.
    pub trace: TraceStore,
    /// Short server identifier included in trace records.
    /// Cloudflare colo code (e.g. `"SJC"`) or operator label (e.g. `"prod-1"`).
    pub server: Option<String>,
    /// Thumbrella build version string.
    pub version: String,
    /// Pre-decoded background image for RGBA compositing (always 250×200 RGB).
    /// Loaded once at startup; `None` falls back to the solid `background_rgb` colour.
    pub background_image: Option<RgbImage>,
    /// Pre-encoded placeholder JPEG returned when no thumbnail can be produced
    /// (unavailable renderer, unsupported format, etc.).
    pub placeholder_general: Vec<u8>,
    /// Pre-encoded placeholder JPEG returned when the request itself fails
    /// (network error, bad URL, HTTP error, etc.).
    pub placeholder_error: Vec<u8>,
    /// Optional in-process renderer registered by a higher tier at startup.
    /// When `Some`, tier-2-routed items are dispatched directly without an
    /// out-of-process HTTP round-trip.
    #[cfg(feature = "native")]
    pub renderer: Option<SharedRenderer>,
    /// I/O and decode budget limits for the shortcut pipeline.
    /// Defaults to [`ShortcutLimits::TIER1`]; override with
    /// [`crate::with_shortcut_limits`] in the tier 2 startup hook.
    pub shortcut_limits: ShortcutLimits,
}

impl Runtime {
    pub fn new(
        cache: CacheStore,
        trace: TraceStore,
        server: Option<String>,
        background_image: Option<RgbImage>,
        placeholder_general: Vec<u8>,
        placeholder_error: Vec<u8>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache,
            trace,
            server,
            version: env!("CARGO_PKG_VERSION").to_string(),
            background_image,
            placeholder_general,
            placeholder_error,
            #[cfg(feature = "native")]
            renderer: None,
            shortcut_limits: ShortcutLimits::TIER1,
        })
    }
}

// ── Portable handoff sub-structs ──────────────────────────────────────────────
//
// These three structs are the only parts of ThumbCook that cross tier
// boundaries.  ThumbHandoff (in handoff.rs) is composed entirely of them, so
// to_handoff() is three clones and from_handoff() is three field assignments.

/// Caller-supplied inputs.  Set at construction; never mutated by the pipeline.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InputSpec {
    /// Source URL to fetch and thumbnail.
    pub url: String,
    /// Caller's prior etag for conditional fetch.
    pub etag: Option<String>,
    /// Allow `file://` URLs.  Only the CLI sets this to `true`.
    pub allow_local: bool,
}

impl InputSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Self::default() }
    }
}

/// Type information discovered during `inspect`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MediaInfo {
    /// Sniffed MIME type string, e.g. `"image/jpeg"`.
    pub mime: Option<String>,
    /// Coarse media category.
    pub kind: Option<FileKind>,
    /// Canonical file extension, no dot (e.g. `"jpeg"`, `"png"`, `"pdf"`).
    pub extension: Option<String>,
    /// Format-specific properties (dimensions, color depth, …).
    pub properties: Option<Value>,
    /// `Content-Length` from the server, if provided.
    pub file_size: Option<u64>,
}

/// Source identity used for cache keying and conditional requests.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SourceIdentity {
    /// URL after following redirects.
    pub final_url: Option<String>,
    /// Query-stripped stable cache key URL.
    pub canonical_url: Option<String>,
    /// Upstream ETag or Last-Modified in opaque `E…`/`M…` prefixed form.
    pub etag: Option<String>,
    /// SHA-256(customer_id ":" content_identity) — the cache storage key.
    pub cache_key: Option<String>,
    /// Which header (or fallback) was used as the identity input for `cache_key`.
    pub cache_key_source: Option<String>,
}

// ── ThumbCook ─────────────────────────────────────────────────────────────────

/// Flat processing buffer for one thumbnail.
///
/// All state needed to drive a thumbnail from input URL to encoded JPEG lives
/// here as directly accessible public fields.  Pipeline steps read and write
/// them without going through accessors.
///
/// Generic over `S: HttpStream` so the same code runs on native (reqwest) and
/// Cloudflare Workers (workers-rs fetch).  Use the `ThumbCook` type alias
/// defined in `lib.rs` for native-only code.
pub struct ThumbCook<S: HttpStream> {

    // ── Pipeline gate ─────────────────────────────────────────────────────────
    /// Current pipeline state.  Steps check `status.is_processing()` before
    /// doing work.  Set to a terminal variant to stop the pipeline.
    pub status: CookStatus,

    // ── Shared runtime ────────────────────────────────────────────────────────
    pub runtime: Arc<Runtime>,

    // ── Handoff-portable groups ───────────────────────────────────────────────
    /// Caller-supplied inputs.  Never mutated after construction.
    pub input: InputSpec,
    /// Sniffed type information populated during `inspect`.
    pub media: MediaInfo,
    /// Source identity and cache key populated during `connect`.
    pub src: SourceIdentity,

    // ── HTTP connection metadata ──────────────────────────────────────────────
    /// Response headers captured on `connect`.
    pub http_headers: HashMap<String, String>,
    /// HTTP status code of the response.
    pub http_status: u16,
    /// Whether the server supports byte-range requests.
    pub http_accepts_ranges: bool,
    // Live connection — access via the http_* methods below.
    http_buf: Option<HttpBuffer<S>>,

    // ── Render state ──────────────────────────────────────────────────────────
    /// Decoded pixel buffer.  Populated by shortcut or render; consumed and
    /// cleared to `None` by `deliver`.
    pub render_image: Option<DynamicImage>,
    /// Pixel dimensions `[width, height]` of the decoded buffer.
    pub render_resolution: Option<[u32; 2]>,
    /// Which renderer handled this item.
    pub render_handler: RenderHandler,
    /// Low-level renderer label (e.g. `"shortcut/exif"`, `"image_crate"`).
    pub render_renderer: Option<String>,
    /// Codec or container detail (e.g. `"h264"`, `"deflate"`).
    pub render_codec: Option<String>,
    /// Video seek offset in seconds (video only).
    pub render_video_seek_secs: Option<f64>,

    // ── Output fields — written as steps complete ────────────────────────────
    /// The encoded JPEG thumbnail bytes.
    pub out_thumbnail: Vec<u8>,
    /// Processing strategy that produced the thumbnail.
    pub out_strategy: Option<Strategy>,
    /// Human-readable error/status message; empty on success.
    pub out_message: String,
    /// Stable token identifying the placeholder image, when applicable.
    pub out_placeholder: Option<String>,
    /// Cache outcome — `None` until the cache check runs.
    pub out_cache: Option<CacheOutcome>,
    /// Wall-clock seconds to generate this result.
    pub out_duration: f64,
    /// Bytes read from the source to generate this result.
    pub out_download_bytes: u64,

    // ── Telemetry — per-step timing ───────────────────────────────────────────
    pub tel_connect_secs: f64,
    pub tel_inspect_secs: f64,
    pub tel_shortcut_secs: f64,
    pub tel_decode_secs: f64,
    pub tel_deliver_secs: f64,
    pub tel_io_secs: f64,
    /// Bytes from any tail Range request (e.g. ZIP Central Directory fetch).
    pub tel_download_tail_bytes: u64,
    /// Byte length of the encoded JPEG.
    pub tel_thumbnail_bytes: Option<u64>,

    // ── Attribution / context ─────────────────────────────────────────────────
    /// How this cook was invoked.
    pub ctx_caller: Option<CallerContext>,
    /// Groups multiple trace records from the same inbound batch call.
    pub ctx_session_id: Option<String>,
    /// Customer identifier for billing and quota attribution.
    pub ctx_customer_id: Option<String>,
    /// True if the client connection dropped before this item completed.
    pub ctx_cancelled: bool,

    // Cancel flag — set via request_cancel(), read via cancelled().
    cancel: Arc<AtomicBool>,
}

impl<S: HttpStream> ThumbCook<S> {

    // ── Construction ──────────────────────────────────────────────────────────

    /// Create a new cook for a URL with a shared runtime.
    pub fn new(url: impl Into<String>, runtime: Arc<Runtime>) -> Self {
        Self::from_input(InputSpec::new(url), runtime)
    }

    /// Create a new cook from a fully-specified [`InputSpec`].
    pub fn from_input(input: InputSpec, runtime: Arc<Runtime>) -> Self {
        Self {
            status:  CookStatus::Processing,
            runtime,
            input,
            media:   MediaInfo::default(),
            src:     SourceIdentity::default(),
            http_headers:        HashMap::new(),
            http_status:         0,
            http_accepts_ranges: false,
            http_buf:            None,
            render_image:             None,
            render_resolution:        None,
            render_handler:           RenderHandler::default(),
            render_renderer:          None,
            render_codec:             None,
            render_video_seek_secs:   None,
            out_thumbnail:       Vec::new(),
            out_strategy:        None,
            out_message:         String::new(),
            out_placeholder:     None,
            out_cache:           None,
            out_duration:        0.0,
            out_download_bytes:  0,
            tel_connect_secs:    0.0,
            tel_inspect_secs:    0.0,
            tel_shortcut_secs:   0.0,
            tel_decode_secs:     0.0,
            tel_deliver_secs:    0.0,
            tel_io_secs:         0.0,
            tel_download_tail_bytes: 0,
            tel_thumbnail_bytes: None,
            ctx_caller:      None,
            ctx_session_id:  None,
            ctx_customer_id: None,
            ctx_cancelled:   false,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    // ── Pipeline gate ─────────────────────────────────────────────────────────

    /// Mark the cook as failed with a message and stop the pipeline.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.status = CookStatus::Failed;
        self.out_message = message.into();
    }

    /// Returns `true` if cancellation has been requested.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Signal cooperative cancellation.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    // ── HTTP buffer delegates ─────────────────────────────────────────────────
    //
    // HttpBuffer's internal page cache, cursor, and stream machinery stay
    // encapsulated.  These are the only ways pipeline steps touch the live
    // connection.

    /// `true` if an open HTTP connection is currently held.
    pub fn http_is_open(&self) -> bool {
        self.http_buf.is_some()
    }

    /// Install a newly opened `HttpBuffer`.  Called only by `pipeline::connect`.
    pub(crate) fn http_install(&mut self, buf: HttpBuffer<S>) {
        self.http_buf = Some(buf);
    }

    /// Read `len` bytes starting at `offset` without moving the cursor.
    pub async fn http_read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, crate::http_buf::HttpError> {
        let buf = self.http_buf.as_mut().ok_or_else(|| crate::http_buf::HttpError::Network("no open connection".into()))?;
        buf.read_at(offset, len).await
    }

    /// Read up to `out.len()` bytes from the current cursor position.
    pub async fn http_read(&mut self, out: &mut [u8]) -> Result<usize, crate::http_buf::HttpError> {
        let buf = self.http_buf.as_mut().ok_or_else(|| crate::http_buf::HttpError::Network("no open connection".into()))?;
        buf.read(out).await
    }

    /// Issue a direct Range GET for `len` bytes starting at `start`.
    pub async fn http_fetch_range(&mut self, start: u64, len: usize) -> Result<Vec<u8>, crate::http_buf::HttpError> {
        let buf = self.http_buf.as_mut().ok_or_else(|| crate::http_buf::HttpError::Network("no open connection".into()))?;
        buf.fetch_range(start, len).await
    }

    /// Rewind the read cursor to byte 0.
    pub fn http_rewind(&mut self) {
        if let Some(b) = self.http_buf.as_mut() { b.rewind(); }
    }

    /// Set an artificial EOF limit.
    pub fn http_set_eof(&mut self, len: u64) {
        if let Some(b) = self.http_buf.as_mut() { b.set_eof(len); }
    }

    /// Remove the artificial EOF limit.
    pub fn http_clear_eof(&mut self) {
        if let Some(b) = self.http_buf.as_mut() { b.clear_eof(); }
    }

    /// Enter streaming mode (one-way; new chunks bypass the page cache).
    pub fn http_enter_streaming_mode(&mut self) {
        if let Some(b) = self.http_buf.as_mut() { b.enter_streaming_mode(); }
    }

    /// Effective file length (artificial EOF if set, else Content-Length).
    pub fn http_stream_len(&self) -> Option<u64> {
        self.http_buf.as_ref().and_then(|b| b.stream_len())
    }

    /// Alias for `http_stream_len` — the `Content-Length` or effective file size.
    pub fn http_len(&self) -> Option<u64> {
        self.http_stream_len()
    }

    /// Enter streaming mode on the buffer, snapshot I/O time, take the buffer
    /// out of the cook, and wrap it as a `Box<dyn ReadSeek + Send>`.
    ///
    /// Returns `None` when no connection is open.
    #[cfg(feature = "native")]
    pub fn http_take_reader(&mut self) -> Option<Box<dyn crate::http_buf::ReadSeek + Send>>
    where
        S: Send + 'static,
    {
        let buf = self.http_buf.as_mut()?;
        buf.enter_streaming_mode();
        self.tel_io_secs = buf.io_secs();
        let buf = self.http_buf.take()?;
        Some(Box::new(crate::http_buf::SyncHttpReader::new(buf)))
    }

    /// Total bytes received from the network so far.
    pub fn http_bytes_fetched(&self) -> u64 {
        self.http_buf.as_ref().map(|b| b.bytes_fetched()).unwrap_or(0)
    }

    /// Cumulative time spent blocked on network I/O since the buffer was opened (excludes connect).
    pub fn http_io_secs(&self) -> f64 {
        self.http_buf.as_ref().map(|b| b.io_secs()).unwrap_or(0.0)
    }

    /// Close the connection and return the first cached page (bytes 0..PAGE_SIZE)
    /// if one was read, for forwarding as a handoff head-start.
    /// Sets `http_buf` to `None`.
    pub async fn http_close(&mut self) -> Option<Vec<u8>> {
        let first_page = if let Some(b) = self.http_buf.as_mut() {
            // Snapshot I/O time before the buffer is dropped.
            self.tel_io_secs = b.io_secs();
            b.close().await
        } else {
            None
        };
        self.http_buf = None;
        first_page
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Snapshot `http_bytes_fetched()` into `out_download_bytes` unless a step
    /// has already set a more precise value.
    pub fn stamp_download_bytes(&mut self) {
        let fetched = self.http_bytes_fetched();
        if self.out_download_bytes == 0 && fetched > 0 {
            self.out_download_bytes = fetched;
        }
        self.tel_io_secs = self.http_io_secs();
    }

    // ── Output views ──────────────────────────────────────────────────────────

    /// Materialise the client-facing [`ThumbResult`].  Called once at end of `run()`.
    pub fn to_result(&self) -> ThumbResult {
        let status = match self.status {
            CookStatus::Processing | CookStatus::Complete => JobStatus::Success,
            CookStatus::NotModified => JobStatus::NotModified,
            CookStatus::Failed      => JobStatus::Failed,
            CookStatus::DeferUser   => JobStatus::DeferUser,
            CookStatus::Unavailable => JobStatus::Unavailable,
            CookStatus::Rendering   => JobStatus::Rendering,
        };
        ThumbResult {
            url:           self.input.url.clone(),
            status,
            duration:      self.out_duration,
            download_size: self.out_download_bytes,
            message:       if self.out_message.is_empty() { None } else { Some(self.out_message.clone()) },
            strategy:      self.out_strategy,
            etag:          self.src.etag.clone(),
            thumbnail:     self.out_thumbnail.clone(),
            placeholder:   self.out_placeholder.clone(),
            mime:          self.media.mime.clone(),
            file_size:     self.media.file_size,
            kind:          self.media.kind,
            extension:     self.media.extension.clone(),
            properties:    self.media.properties.clone().unwrap_or_else(|| Value::Object(Default::default())),
            cache:         self.out_cache.map(|c| c.public_label().to_string()),
        }
    }

    /// Materialise the internal [`ThumbTrace`] for the log sink.  Called once
    /// at end of `run()`, after `to_result()`.
    pub fn to_trace(&self) -> ThumbTrace {
        #[cfg(feature = "native")]
        let timestamp = {
            use time::OffsetDateTime;
            use time::format_description::well_known::Rfc3339;
            OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
        };
        #[cfg(not(feature = "native"))]
        let timestamp = String::new();

        let status = match self.status {
            CookStatus::Processing | CookStatus::Complete => JobStatus::Success,
            CookStatus::NotModified => JobStatus::NotModified,
            CookStatus::Failed      => JobStatus::Failed,
            CookStatus::DeferUser   => JobStatus::DeferUser,
            CookStatus::Unavailable => JobStatus::Unavailable,
            CookStatus::Rendering   => JobStatus::Rendering,
        };

        ThumbTrace {
            timestamp,
            status,
            strategy:            self.out_strategy,
            kind:                self.media.kind,
            extension:           self.media.extension.clone(),
            canonical_url:       self.src.canonical_url.clone(),
            final_url:           self.src.final_url.clone(),
            cache_key:           self.src.cache_key.clone(),
            cache_key_source:    self.src.cache_key_source.clone(),
            source_etag:         self.src.etag.clone(),
            download_bytes:      self.out_download_bytes,
            download_tail_bytes: self.tel_download_tail_bytes,
            connect_secs:        self.tel_connect_secs,
            io_secs:             self.tel_io_secs,
            inspect_secs:        self.tel_inspect_secs,
            shortcut_secs:       self.tel_shortcut_secs,
            decode_secs:         self.tel_decode_secs,
            deliver_secs:        self.tel_deliver_secs,
            render_resolution:   self.render_resolution,
            thumbnail_bytes:     self.tel_thumbnail_bytes,
            job_tier:            1,
            job_renderer:        self.render_renderer.clone(),
            job_codec:           self.render_codec.clone(),
            video_seek_secs:     self.render_video_seek_secs,
            session_id:          self.ctx_session_id.clone(),
            customer_id:         self.ctx_customer_id.clone(),
            cache_hit:           self.out_cache,
            render_handler:      self.render_handler,
            caller:              self.ctx_caller.clone(),
            cancelled:           self.ctx_cancelled,
            server:              self.runtime.server.clone(),
            version:             self.runtime.version.clone(),
        }
    }

    /// Project into a [`ThumbHandoff`] for forwarding to a higher-tier renderer.
    ///
    /// `first_page` is the return value of [`http_close`] — the first cached
    /// page, forwarded so the receiver can start parsing without a new request.
    pub fn to_handoff(&self, first_page: Option<Vec<u8>>) -> crate::handoff::ThumbHandoff {
        crate::handoff::ThumbHandoff {
            input:      self.input.clone(),
            media:      self.media.clone(),
            src:        self.src.clone(),
            first_page,
        }
    }

    // ── Pipeline entry ────────────────────────────────────────────────────────

    /// Run the full pipeline and return `(result, trace, after)`.
    ///
    /// `after` holds deferred cache-write futures.  Callers must drain it
    /// after the HTTP response is sent:
    /// - Native: `after.drain_spawn()` fires all tasks on the tokio thread pool.
    /// - Workers: iterate `after.drain()` and pass each to `ctx.wait_until()`.
    pub async fn run(mut self) -> (ThumbResult, ThumbTrace, AfterResponse)
    where
        S: Send + 'static,
    {
        let mut after = AfterResponse::new();
        let t0 = std::time::Instant::now();

        // ── connect ───────────────────────────────────────────────────────────
        let t_step = std::time::Instant::now();
        pipeline::connect(&mut self).await;
        self.tel_connect_secs = t_step.elapsed().as_secs_f64();
        if !self.status.is_processing() {
            self.stamp_download_bytes();
            self.out_duration = t0.elapsed().as_secs_f64();
            return self.finish(after);
        }

        // ── cache key derivation ──────────────────────────────────────────────
        {
            use sha2::{Sha256, Digest};
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            use crate::source::content_hash_from_headers;

            let (identity, source) = content_hash_from_headers(&self.http_headers)
                .map(|(hash, src)| (hash, src.to_string()))
                .unwrap_or_else(|| {
                    let url = self.src.final_url.as_deref()
                        .or(self.src.canonical_url.as_deref())
                        .unwrap_or(self.input.url.as_str());
                    (url.to_string(), "url".to_string())
                });

            let key_input = format!(
                "v{}:{}:{identity}",
                crate::TBR_CACHE_VERSION,
                self.ctx_customer_id.as_deref().unwrap_or("")
            );
            self.src.cache_key        = Some(URL_SAFE_NO_PAD.encode(Sha256::digest(key_input.as_bytes())));
            self.src.cache_key_source = Some(source);
        }

        // ── cache check ───────────────────────────────────────────────────────
        if let Some(ref key) = self.src.cache_key.clone() {
            if let Some(cached) = self.runtime.cache.check(key).await {
                self.http_close().await;
                self.out_thumbnail     = cached.thumbnail;
                self.out_strategy      = cached.strategy;
                self.out_message       = cached.message.unwrap_or_default();
                self.out_placeholder   = cached.placeholder;
                self.out_download_bytes = cached.download_size;
                self.media.mime        = cached.mime;
                self.media.file_size   = cached.file_size;
                self.media.kind        = cached.kind;
                self.media.extension   = cached.extension;
                self.media.properties  = Some(cached.properties);
                self.out_cache         = Some(CacheOutcome::File);
                self.status            = CookStatus::Complete;
                self.out_duration      = t0.elapsed().as_secs_f64();
                return self.finish(after);
            }
        }

        // ── inspect ───────────────────────────────────────────────────────────
        let t_step = std::time::Instant::now();
        pipeline::inspect(&mut self).await;
        self.tel_inspect_secs = t_step.elapsed().as_secs_f64();
        if !self.status.is_processing() {
            self.stamp_download_bytes();
            self.out_duration = t0.elapsed().as_secs_f64();
            return self.finish(after);
        }

        // ── shortcut ──────────────────────────────────────────────────────────
        let t_step = std::time::Instant::now();
        pipeline::shortcut(&mut self).await;
        self.tel_shortcut_secs = t_step.elapsed().as_secs_f64();
        self.stamp_download_bytes();

        if self.status == CookStatus::Complete {
            self.out_duration = t0.elapsed().as_secs_f64();
            if let Some(ref key) = self.src.cache_key.clone() {
                let result = self.to_result();
                self.runtime.cache.store(key, &result, &mut after);
            }
            return self.finish(after);
        }

        // ── deliver (when shortcut decoded an image) ──────────────────────────
        if self.render_image.is_some() {
            let t_step = std::time::Instant::now();
            pipeline::deliver(&mut self).await;
            self.tel_deliver_secs = t_step.elapsed().as_secs_f64();
            self.out_duration = t0.elapsed().as_secs_f64();
            if self.status == CookStatus::Complete {
                if let Some(ref key) = self.src.cache_key.clone() {
                    let result = self.to_result();
                    self.runtime.cache.store(key, &result, &mut after);
                }
            }
            return self.finish(after);
        }

        // ── handoff to higher tier ────────────────────────────────────────────
        // Check for a registered in-process renderer *before* closing the
        // connection so the renderer can stream from the live HttpBuffer.
        #[cfg(feature = "native")]
        if let Some(renderer) = self.runtime.renderer.clone() {
            // Snapshot bytes-fetched so far (inspect/shortcut phase).
            // The renderer takes the live buffer via RenderCook::take_reader;
            // after that point the buffer is gone and we can't query it.
            self.stamp_download_bytes();

            // Coerce &mut ThumbCook<S> → &mut dyn RenderCook.  Valid because
            // impl<S: HttpStream + Send + 'static> RenderCook for ThumbCook<S>
            // and run() requires S: Send + 'static.
            if renderer.render(&mut self as &mut dyn RenderCook).await {
                // Renderer claimed the format.  On success render_image is set
                // and deliver produces the thumbnail.  On failure the renderer
                // called fail_cook() and render_image is None; status != Complete.

                // Best-effort download size when the renderer consumed bytes
                // we can no longer count via the buffer.
                if self.out_download_bytes == 0 {
                    if let Some(sz) = self.media.file_size {
                        self.out_download_bytes = sz;
                    }
                }

                if self.render_image.is_some() {
                    let t_step = std::time::Instant::now();
                    pipeline::deliver(&mut self).await;
                    self.tel_deliver_secs = t_step.elapsed().as_secs_f64();
                    self.out_duration = t0.elapsed().as_secs_f64();
                    if self.status == CookStatus::Complete {
                        if let Some(ref key) = self.src.cache_key.clone() {
                            let result = self.to_result();
                            self.runtime.cache.store(key, &result, &mut after);
                        }
                    }
                }
                return self.finish(after);
            }
            // Renderer returned false (format not recognised).
            // It must not have called take_reader() in this case.
            self.status = CookStatus::Unavailable;
            self.out_message = "format not handled by the registered renderer".to_string();
            self.out_duration = t0.elapsed().as_secs_f64();
            return self.finish(after);
        }

        // No in-process renderer — close the HTTP connection and build a
        // portable handoff bundle for an out-of-process tier-2 endpoint.
        let first_page = self.http_close().await;
        let _handoff = self.to_handoff(first_page);
        // TODO: out-of-process HTTP handoff to a tier-2 endpoint.
        self.status = CookStatus::Unavailable;
        self.out_message = "no higher-tier renderer is configured".to_string();
        self.out_duration = t0.elapsed().as_secs_f64();
        self.finish(after)
    }

    fn finish(mut self, mut after: AfterResponse) -> (ThumbResult, ThumbTrace, AfterResponse) {
        // Final snapshot of I/O counters — safe to call multiple times since
        // stamp_download_bytes is idempotent on the bytes field.
        self.stamp_download_bytes();

        // Always return a thumbnail.  If the pipeline didn't produce one,
        // fill in the appropriate placeholder JPEG.
        if self.out_thumbnail.is_empty() {
            let (bytes, label) = match self.status {
                CookStatus::Failed => (
                    self.runtime.placeholder_error.clone(),
                    "error",
                ),
                _ => (
                    self.runtime.placeholder_general.clone(),
                    "general",
                ),
            };
            if !bytes.is_empty() {
                self.out_thumbnail  = bytes;
                self.out_placeholder = Some(label.to_string());
            }
        }

        let result = self.to_result();
        let trace  = self.to_trace();
        self.runtime.trace.record(trace.clone(), &mut after);
        (result, trace, after)
    }
}

// ── RenderCook impl ───────────────────────────────────────────────────────────
//
// Lives in cook.rs (not renderer.rs) so it can access private fields.
// `S: Send + 'static` is required for `&mut ThumbCook<S>` to coerce to
// `&mut dyn RenderCook`.

#[cfg(feature = "native")]
impl<S: HttpStream + Send + 'static> crate::renderer::RenderCook for ThumbCook<S> {
    fn media_kind(&self) -> Option<crate::media::FileKind> {
        self.media.kind
    }
    fn media_extension(&self) -> Option<&str> {
        self.media.extension.as_deref()
    }
    fn input_url(&self) -> &str {
        &self.input.url
    }
    fn content_length(&self) -> Option<u64> {
        self.http_len()
    }
    fn take_reader(&mut self) -> Option<Box<dyn crate::http_buf::ReadSeek + Send>> {
        self.http_take_reader()
    }
    fn set_render_image(&mut self, img: image::DynamicImage) {
        self.render_image = Some(img);
    }
    fn set_render_renderer(&mut self, label: String) {
        self.render_renderer = Some(label);
    }
    fn set_render_codec(&mut self, codec: String) {
        self.render_codec = Some(codec);
    }
    fn set_render_video_seek_secs(&mut self, secs: f64) {
        self.render_video_seek_secs = Some(secs);
    }
    fn set_media_properties(&mut self, props: serde_json::Value) {
        self.media.properties = Some(props);
    }
    fn fail_cook(&mut self, msg: &str) {
        self.fail(msg);
    }
}
