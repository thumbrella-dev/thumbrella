//! Cook execution context - the central processing state for one thumbnail.
//!
//! Many items in this module are only used by the native HTTP server path and
//! appear dead when building for wasm32 (without the `native` feature).
//! Feature-gating each one would be noisy, so dead_code is suppressed globally.
#![allow(dead_code)]
//!
//! # Design
//!
//! [`ThumbCook`] is a flat buffer of all state needed to process one thumbnail,
//! from initial inputs through to final output.  Pipeline steps read and write
//! fields directly - there are no intermediate aggregation types or synchronisation
//! bridges.
//!
//! Fields are grouped by prefix:
//!
//! | Prefix   | Contents |
//! |----------|----------|
//! | `in_`    | Caller inputs (set at construction, never mutated by pipeline) |
//! | `http_`  | HTTP connection metadata captured during `connect` |
//! | `media_` | Sniffed type info populated during `inspect` |
//! | `src_`   | Source identity - cache key, canonical URL, etag |
//! | `render_`| Pixel work state - decoded buffer, resolution, codec info |
//! | `out_`   | Client-facing output fields written as steps complete |
//! | `tel_`   | Per-step timing telemetry |
//! | `ctx_`   | Attribution / request context (caller, session, customer) |
//!
//! # Output views
//!
//! Neither [`ThumbResult`] nor [`ThumbTrace`] exist during processing.  They
//! are materialised only at two points:
//! - [`ThumbCook::to_result`] - called once to cache and return to the client.
//! - [`ThumbCook::to_trace`] - called once to emit to the configured log sink.
//!
//! # Tier handoff
//!
//! [`ThumbCook::take_handoff`](crate::cook::ThumbCook::take_handoff) projects
//! the three portable sub-structs ([`InputSpec`], [`MediaInfo`],
//! [`SourceIdentity`]) into a [`crate::handoff::ThumbHandoff`] for
//! serialisation and forwarding to a higher-tier renderer.
//! [`ThumbCook::from_handoff`] reconstructs the cook on the receiving tier,
//! populating those same fields and setting `status = Processing` so the
//! pipeline enters at the render step.
//!
//! # Pipeline gate
//!
//! Steps check `cook.status == CookStatus::Processing` to decide whether to
//! continue.  Any step that encounters a terminal condition sets `cook.status`
//! to the appropriate [`CookStatus`] variant and returns - no other sentinel
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
use crate::cache::{CacheStore, render_cost_from_secs};
use crate::fetch_guard::{OriginBackoffCache, UrlFailureCache};
use crate::handoff::{HandoffInflight, HandoffResponse};
use crate::http_buf::{HttpBuffer, HttpStream};
use crate::media::FileKind;
use crate::pipeline;
#[cfg(feature = "native")]
use crate::renderer::{RenderCook, SharedRenderer};
use crate::result::{RenderHandler, ResultSource, ResultStatus, ThumbMedia, ThumbResult, ThumbTrace};
use crate::source::CacheHints;
use crate::spec::ShortcutLimits;
use crate::tracelog::TraceStore;

//  CookStatus

/// Internal pipeline gate.  Steps check `cook.status == CookStatus::Processing`
/// to decide whether to continue.  Any step that hits a terminal condition sets
/// this and returns immediately; no other sentinel is needed.
///
/// Maps to [`ResultStatus`] via [`ThumbCook::to_result`] for the client view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CookStatus {
    /// Pipeline is running - the only "keep going" state.
    #[default]
    Processing,
    /// Thumbnail produced successfully (may be a placeholder).
    Complete,
    /// Conditional request; source unchanged since caller's supplied validator.
    Fresh,
    /// Terminal failure - message is in `out_message`.
    Failed,
    /// Server is at capacity; client should retry later.
    Overloaded,
    /// Handed off to a higher-tier renderer; streaming result pending.
    Intermediate,
}

impl CookStatus {
    /// Whether the pipeline should continue.
    pub fn is_processing(self) -> bool {
        self == Self::Processing
    }
}

//  Runtime

/// Shared server-wide configuration.  One instance per process, referenced by
/// every concurrent cook via `Arc<Runtime>`.
///
/// On `wasm32-unknown-unknown` (Cloudflare Workers) `Arc` is zero-cost - the
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
    // Placeholder JPEGs are no longer stored in Runtime.  They are embedded
    // as `&'static [u8]` in `crate::assets::placeholders` and selected
    // per-kind at response time via `crate::assets::placeholder_for_kind`.
    /// Optional in-process renderer registered by a higher tier at startup.
    /// When `Some`, tier-2-routed items are dispatched directly without an
    /// out-of-process HTTP round-trip.
    #[cfg(feature = "native")]
    pub renderer: Option<SharedRenderer>,
    /// Tier-2 handoff connect target (URL + optional headers).
    pub handoff_tier2: crate::connect::ConnectTarget,
    /// Tier-3 handoff connect target (URL + optional headers).
    pub handoff_tier3: crate::connect::ConnectTarget,
    /// Shared secret required on all endpoints when set.
    /// If `None`, the server is publicly accessible.
    pub handshake: Option<String>,
    /// Allow `file://` URLs and bare absolute paths in HTTP endpoint requests.
    ///
    /// Set from [`crate::config::AppConfig::allow_local`] (`TBR_ALLOW_LOCAL`).
    /// Propagated
    /// to each [`InputSpec`] by the route handlers.  The second-line guard
    /// in `pipeline::connect` also checks `InputSpec::allow_local` directly.
    pub allow_local: bool,
    /// I/O and decode budget limits for the shortcut pipeline.
    /// Defaults to [`ShortcutLimits::TIER1`]; override with
    /// [`crate::with_shortcut_limits`] in the tier 2 startup hook.
    pub shortcut_limits: ShortcutLimits,
    /// Value sent as the `User-Agent` request header on all outbound fetches.
    pub user_agent: String,
    /// Short-lived debounce cache for URLs that recently returned 4xx / 5xx.
    pub url_failures: UrlFailureCache,
    /// Rate-control cache for origins that returned 429 / 503.
    pub origin_backoff: OriginBackoffCache,
    /// Default origin back-off TTL (seconds) used when no `Retry-After` header
    /// is present in a 429 / 503 response.
    pub backoff_default: u64,
    /// Maximum server-side cache TTL in seconds (caps upstream max-age).
    pub cache_max_ttl_secs: u64,
    /// Default cache TTL when upstream provides no freshness hints.
    pub cache_default_ttl_secs: u64,
    /// Single-flight deduplication for concurrent handoffs to the same cache key.
    pub handoff_inflight: HandoffInflight,
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cache: CacheStore,
        trace: TraceStore,
        server: Option<String>,
        background_image: Option<RgbImage>,
        handoff_tier2: crate::connect::ConnectTarget,
        handoff_tier3: crate::connect::ConnectTarget,
        handshake: Option<String>,
        allow_local: bool,
        failure_ttl: u64,
        backoff_default: u64,
        backoff_ceiling: u64,
        cache_max_ttl_secs: u64,
        cache_default_ttl_secs: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache,
            trace,
            server,
            version: env!("CARGO_PKG_VERSION").to_string(),
            background_image,
            #[cfg(feature = "native")]
            renderer: None,
            handoff_tier2,
            handoff_tier3,
            handshake,
            allow_local,
            shortcut_limits: ShortcutLimits::TIER1,
            user_agent: format!("Thumbrella/{}", env!("CARGO_PKG_VERSION")),
            url_failures: UrlFailureCache::new(failure_ttl),
            origin_backoff: OriginBackoffCache::new(backoff_ceiling),
            backoff_default,
            cache_max_ttl_secs,
            cache_default_ttl_secs,
            handoff_inflight: HandoffInflight::new(),
        })
    }
}

//  Portable handoff sub-structs
//
// These three structs are the only parts of ThumbCook that cross tier
// boundaries.  ThumbHandoff (in handoff.rs) is composed entirely of them, so
// to_handoff() is three clones and from_handoff() is three field assignments.

/// Caller-supplied inputs.  Set at construction; never mutated by the pipeline.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InputSpec {
    /// Source URL to fetch and thumbnail.
    pub url: String,
    /// Caller's prior cache hints for conditional fetch and client-side freshness.
    pub cache: Option<CacheHints>,
    /// Allow `file://` URLs.  Only the CLI sets this to `true`.
    pub allow_local: bool,
}

impl InputSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
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
    /// Structured freshness hints parsed from upstream response headers.
    pub cache_hints: Option<CacheHints>,
    /// SHA-256(customer_id ":" content_identity) - the cache storage key.
    pub cache_key: Option<String>,
    /// Which header (or fallback) was used as the identity input for `cache_key`.
    pub cache_key_source: Option<String>,
}

//  ThumbCook

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
    //  Pipeline gate
    /// Current pipeline state.  Steps check `status.is_processing()` before
    /// doing work.  Set to a terminal variant to stop the pipeline.
    pub status: CookStatus,

    //  Shared runtime
    pub runtime: Arc<Runtime>,

    //  Handoff-portable groups
    /// Caller-supplied inputs.  Never mutated after construction.
    pub input: InputSpec,
    /// Sniffed type information populated during `inspect`.
    pub media: MediaInfo,
    /// Source identity and cache key populated during `connect`.
    pub src: SourceIdentity,

    //  HTTP connection metadata
    /// Response headers captured on `connect`.
    pub http_headers: HashMap<String, String>,
    /// HTTP status code of the response.
    pub http_status: u16,
    /// Whether the server supports byte-range requests.
    pub http_accepts_ranges: bool,
    // Live connection - access via the http_* methods below.
    http_buf: Option<HttpBuffer<S>>,

    //  Render state
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
    /// True when `render_image` is a partial progressive JPEG decode (low-res
    /// intermediate). Used to suppress pixel-art heuristic since the small size
    /// is an artifact of partial decoding, not genuine small-image content.
    pub render_is_progressive_partial: bool,

    //  Output fields - written as steps complete
    /// The encoded JPEG thumbnail bytes.
    pub out_thumbnail: Vec<u8>,
    /// Human-readable error/status message; empty on success.
    pub out_message: String,
    /// Stable token identifying the placeholder image, when applicable.
    pub out_placeholder: Option<String>,
    /// When set, overrides the source in `to_result()` for placeholder paths.
    pub placeholder_source: Option<ResultSource>,
    /// Cache outcome - `None` until the cache check runs.
    pub cache_hit: Option<String>,
    /// Wall-clock seconds to generate this result.
    pub out_duration: f64,
    /// Bytes read from the source to generate this result.
    pub out_download_bytes: u64,
    /// Actual bytes consumed by the renderer (set via RenderCook::set_bytes_consumed).
    /// When Some, this overrides the file_size fallback in the download counter.
    render_bytes_consumed: Option<u64>,

    //  Telemetry - per-step timing
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
    /// Override trace tier when the resolver tier is a handoff target.
    pub tel_job_tier_override: Option<u8>,
    /// Override trace version when the resolver tier is a handoff target.
    pub tel_version_override: Option<String>,

    //  Attribution / context
    /// Groups multiple trace records from the same inbound batch call.
    pub ctx_session_id: Option<String>,
    /// Customer identifier for billing and quota attribution.
    pub ctx_customer_id: Option<String>,
    /// True if the client connection dropped before this item completed.
    pub ctx_cancelled: bool,
    /// True when this cook was reconstructed from a higher-tier handoff.
    /// Handoff cooks skip cache and inspect/shortcut stages.
    pub ctx_handoff: bool,

    /// Set to true after `resolve_cache` runs (whether it resolved or not).
    /// Prevents `run_with_progress` from re-running the cache phase.
    pub cache_resolved: bool,

    /// When true, skip the handoff/render chain after shortcut+deliver.
    /// The pipeline still runs cache checks, connect, inspect, shortcut,
    /// and deliver - only expensive render/handoff is gated.  Set by the
    /// cloud wrapper when an account is over its render quota.
    pub render_disabled: bool,

    /// When set, shortcut-produced results and over-quota placeholders use
    /// this TTL (seconds from now) for both cache storage expiry and client
    /// freshness hints.  Computed by the cloud wrapper as the time until
    /// the quota window resets (end of hour/day).
    pub render_disabled_ttl_secs: Option<u64>,

    // Cancel flag - set via request_cancel(), read via cancelled().
    cancel: Arc<AtomicBool>,
}

impl<S: HttpStream> ThumbCook<S> {
    //  Construction

    /// Create a new cook for a URL with a shared runtime.
    pub fn new(url: impl Into<String>, runtime: Arc<Runtime>) -> Self {
        Self::from_input(InputSpec::new(url), runtime)
    }

    /// Create a new cook from a fully-specified [`InputSpec`].
    pub fn from_input(input: InputSpec, runtime: Arc<Runtime>) -> Self {
        Self {
            status: CookStatus::Processing,
            runtime,
            input,
            media: MediaInfo::default(),
            src: SourceIdentity::default(),
            http_headers: HashMap::new(),
            http_status: 0,
            http_accepts_ranges: false,
            http_buf: None,
            render_image: None,
            render_resolution: None,
            render_handler: RenderHandler::default(),
            render_renderer: None,
            render_codec: None,
            render_video_seek_secs: None,
            render_is_progressive_partial: false,
            out_thumbnail: Vec::new(),
            out_message: String::new(),
            out_placeholder: None,
            placeholder_source: None,
            cache_hit: None,
            out_duration: 0.0,
            out_download_bytes: 0,
            render_bytes_consumed: None,
            tel_connect_secs: 0.0,
            tel_inspect_secs: 0.0,
            tel_shortcut_secs: 0.0,
            tel_decode_secs: 0.0,
            tel_deliver_secs: 0.0,
            tel_io_secs: 0.0,
            tel_download_tail_bytes: 0,
            tel_thumbnail_bytes: None,
            tel_job_tier_override: None,
            tel_version_override: None,
            ctx_session_id: None,
            ctx_customer_id: None,
            ctx_cancelled: false,
            ctx_handoff: false,
            cache_resolved: false,
            render_disabled: false,
            render_disabled_ttl_secs: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Reconstruct a cook from a serialized handoff payload.
    pub fn from_handoff(handoff: crate::handoff::ThumbHandoff, runtime: Arc<Runtime>) -> Self {
        let mut cook = Self::from_input(handoff.input, runtime);
        cook.media = handoff.media;
        cook.src = handoff.src;
        cook.ctx_handoff = true;
        cook
    }

    //  Pipeline gate

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

    //  HTTP buffer delegates
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
    pub async fn http_read_at(
        &mut self,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, crate::http_buf::HttpError> {
        let buf = self
            .http_buf
            .as_mut()
            .ok_or_else(|| crate::http_buf::HttpError::Network("no open connection".into()))?;
        buf.read_at(offset, len).await
    }

    /// Read up to `out.len()` bytes from the current cursor position.
    pub async fn http_read(&mut self, out: &mut [u8]) -> Result<usize, crate::http_buf::HttpError> {
        let buf = self
            .http_buf
            .as_mut()
            .ok_or_else(|| crate::http_buf::HttpError::Network("no open connection".into()))?;
        buf.read(out).await
    }

    /// Issue a direct Range GET for `len` bytes starting at `start`.
    pub async fn http_fetch_range(
        &mut self,
        start: u64,
        len: usize,
    ) -> Result<Vec<u8>, crate::http_buf::HttpError> {
        let buf = self
            .http_buf
            .as_mut()
            .ok_or_else(|| crate::http_buf::HttpError::Network("no open connection".into()))?;
        buf.fetch_range(start, len).await
    }

    /// Rewind the read cursor to byte 0.
    pub fn http_rewind(&mut self) {
        if let Some(b) = self.http_buf.as_mut() {
            b.rewind();
        }
    }

    /// Set an artificial EOF limit.
    pub fn http_set_eof(&mut self, len: u64) {
        if let Some(b) = self.http_buf.as_mut() {
            b.set_eof(len);
        }
    }

    /// Remove the artificial EOF limit.
    pub fn http_clear_eof(&mut self) {
        if let Some(b) = self.http_buf.as_mut() {
            b.clear_eof();
        }
    }

    /// Enter streaming mode (one-way; new chunks bypass the page cache).
    pub fn http_enter_streaming_mode(&mut self) {
        if let Some(b) = self.http_buf.as_mut() {
            b.enter_streaming_mode();
        }
    }

    /// Effective file length (artificial EOF if set, else Content-Length).
    pub fn http_stream_len(&self) -> Option<u64> {
        self.http_buf.as_ref().and_then(|b| b.stream_len())
    }

    /// Alias for `http_stream_len` - the `Content-Length` or effective file size.
    pub fn http_len(&self) -> Option<u64> {
        self.http_stream_len()
    }

    /// Snapshot I/O time, take the buffer out of the cook, and wrap it as a
    /// `Box<dyn ReadSeek + Send>`.
    ///
    /// Returns `None` when no connection is open.
    #[cfg(feature = "native")]
    pub fn http_take_reader(&mut self) -> Option<Box<dyn crate::http_buf::ReadSeek + Send>>
    where
        S: Send + 'static,
    {
        let buf = self.http_buf.as_mut()?;
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

    //  Helpers

    /// Snapshot `http_bytes_fetched()` into `out_download_bytes` unless a step
    /// has already set a more precise value.
    pub fn stamp_download_bytes(&mut self) {
        let fetched = self.http_bytes_fetched();
        if self.out_download_bytes == 0 && fetched > 0 {
            self.out_download_bytes = fetched;
        }
        self.tel_io_secs = self.http_io_secs();
    }

    //  Output views

    /// Materialise the client-facing [`ThumbResult`].  Called once at end of `run()`.
    pub fn to_result(&self) -> ThumbResult {
        let status = match self.status {
            CookStatus::Processing | CookStatus::Complete => ResultStatus::Success,
            CookStatus::Fresh => ResultStatus::Success,
            CookStatus::Failed => ResultStatus::Failed,
            CookStatus::Overloaded => ResultStatus::Overloaded,
            CookStatus::Intermediate => ResultStatus::Intermediate,
        };
        ThumbResult {
            url: self.input.url.clone(),
            status,
            duration: self.out_duration,
            download_size: self.out_download_bytes,
            message: if self.out_message.is_empty() { None } else { Some(self.out_message.clone()) },
            http_status: if self.http_status > 0 { Some(self.http_status) } else { None },
            source: if self.status == CookStatus::Fresh {
                Some(ResultSource::NotModified)
            } else if let Some(ps) = self.placeholder_source {
                Some(ps)
            } else if self.out_placeholder.is_some() {
                Some(ResultSource::Fallback)
            } else if self.render_renderer.as_deref().is_some_and(|r| r.starts_with("shortcut/")) {
                Some(ResultSource::Shortcut)
            } else {
                Some(ResultSource::Render)
            },
            media: Some(ThumbMedia {
                url: self.input.url.clone(),
                thumbnail: self.out_thumbnail.clone(),
                mime: self.media.mime.clone().unwrap_or_default(),
                file_size: self.media.file_size.unwrap_or(0),
                kind: self.media.kind.unwrap_or(FileKind::Unknown),
                extension: crate::pipeline::canonical_extension(
                    &self.media.extension.clone().unwrap_or_default(),
                ),
                properties: self
                    .media
                    .properties
                    .clone()
                    .unwrap_or_else(|| Value::Object(Default::default())),
                placeholder: self.out_placeholder.clone().unwrap_or_default(),
                cache: self
                    .src
                    .cache_hints
                    .as_ref()
                    .map(|h| h.encode(self.runtime.cache_default_ttl_secs))
                    .unwrap_or_default(),
            }),
        }
    }

    /// Materialise an intermediate client-facing result while processing is still in flight.
    ///
    /// Used by streaming batch responses to send a placeholder-backed snapshot
    /// after inspect/shortcut and before render or handoff begins.
    pub fn to_progress_result(&self, duration: f64) -> ThumbResult {
        let kind = self.media.kind.unwrap_or(FileKind::Unknown);
        ThumbResult {
            url: self.input.url.clone(),
            status: ResultStatus::Intermediate,
            duration,
            download_size: self.out_download_bytes.max(self.http_bytes_fetched()),
            message: None,
            http_status: if self.http_status > 0 { Some(self.http_status) } else { None },
            source: None,
            media: Some(ThumbMedia {
                url: self.input.url.clone(),
                thumbnail: crate::assets::placeholder_for_kind(kind).to_vec(),
                mime: self.media.mime.clone().unwrap_or_default(),
                file_size: self.media.file_size.unwrap_or(0),
                kind,
                extension: crate::pipeline::canonical_extension(
                    &self.media.extension.clone().unwrap_or_default(),
                ),
                properties: self
                    .media
                    .properties
                    .clone()
                    .unwrap_or_else(|| Value::Object(Default::default())),
                placeholder: self.out_placeholder.clone().unwrap_or_default(),
                cache: self
                    .src
                    .cache_hints
                    .as_ref()
                    .map(|h| h.encode(self.runtime.cache_default_ttl_secs))
                    .unwrap_or_default(),
            }),
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

        #[cfg(feature = "native")]
        let default_tier = if self.runtime.renderer.is_some() { 2 } else { 1 };
        #[cfg(not(feature = "native"))]
        let default_tier = 1;

        // Shortcut succeeded when it ran (secs > 0) but no full decode followed.
        let shortcut_succeeded = self.tel_shortcut_secs > 0.0 && self.tel_decode_secs == 0.0;

        ThumbTrace {
            timestamp,
            source: if self.status == CookStatus::Fresh {
                Some(ResultSource::NotModified)
            } else if let Some(ps) = self.placeholder_source {
                Some(ps)
            } else if self.out_placeholder.is_some() {
                Some(ResultSource::Fallback)
            } else if self.render_renderer.as_deref().is_some_and(|r| r.starts_with("shortcut/")) {
                Some(ResultSource::Shortcut)
            } else {
                Some(ResultSource::Render)
            },
            kind: self.media.kind,
            extension: self.media.extension.as_deref().map(crate::pipeline::canonical_extension),
            canonical_url: self.src.canonical_url.clone(),
            download_bytes: self.out_download_bytes,
            download_tail_bytes: self.tel_download_tail_bytes,
            io_secs: self.tel_connect_secs + self.tel_io_secs,
            inspect_secs: self.tel_inspect_secs
                + if shortcut_succeeded { 0.0 } else { self.tel_shortcut_secs },
            render_secs: if shortcut_succeeded { self.tel_shortcut_secs } else { self.tel_decode_secs },
            deliver_secs: self.tel_deliver_secs,
            thumbnail_bytes: self.tel_thumbnail_bytes,
            job_tier: self.tel_job_tier_override.unwrap_or(default_tier),
            job_renderer: self.render_renderer.clone(),
            message: if self.out_message.is_empty() { None } else { Some(self.out_message.clone()) },
            server: self.runtime.server.clone(),
            version: self
                .tel_version_override
                .clone()
                .unwrap_or_else(|| self.runtime.version.clone()),
        }
    }

    /// Close the HTTP connection and project into a
    /// [`crate::handoff::ThumbHandoff`] for
    /// forwarding to a higher-tier renderer over an external transport.
    ///
    /// Closing and serialising are combined into a single step so it is
    /// structurally impossible to send a handoff while the connection is
    /// still open - the caller cannot obtain a `ThumbHandoff` without
    /// first relinquishing the live stream.
    pub async fn take_handoff(&mut self) -> crate::handoff::ThumbHandoff
    where
        S: Send + 'static,
    {
        let first_page = self.http_close().await;
        crate::handoff::ThumbHandoff {
            input: self.input.clone(),
            media: self.media.clone(),
            src: self.src.clone(),
            first_page,
        }
    }

    //  Handoff application

    /// Apply the result of a tier handoff to this cook.
    ///
    /// Shared by both the **leader** (returned directly from `post_handoff`) and
    /// **joiners** (received via [`HandoffInflight`]).
    ///
    /// `local_bytes` is the number of bytes this cook fetched from the source
    /// during connect/inspect before the handoff was issued.  It is added to
    /// `remote.result.download_size` to form the complete download count.
    fn apply_handoff_response(&mut self, remote: &HandoffResponse, target_tier: u8, local_bytes: u64) {
        let res = &remote.result;
        let trace = &remote.trace;

        self.status = cook_status_from_job(res.status);
        self.out_message = res.message.clone().unwrap_or_default();
        self.out_placeholder = res
            .media
            .as_ref()
            .and_then(|m| if m.placeholder.is_empty() { None } else { Some(m.placeholder.clone()) });
        self.out_download_bytes = local_bytes.saturating_add(res.download_size);

        if let Some(ref media) = res.media {
            self.out_thumbnail = media.thumbnail.clone();
            self.media.mime = Some(media.mime.clone());
            self.media.file_size = Some(media.file_size);
            self.media.kind = Some(media.kind);
            self.media.extension = Some(crate::pipeline::canonical_extension(&media.extension));
            self.media.properties = Some(media.properties.clone());
        }

        self.tel_job_tier_override = Some(trace.job_tier);
        self.tel_version_override = Some(trace.version.clone());
        if self.render_renderer.is_none() {
            self.render_renderer = trace.job_renderer.clone();
        }
        self.tel_thumbnail_bytes = trace.thumbnail_bytes;
        self.tel_download_tail_bytes = trace.download_tail_bytes;
        self.tel_decode_secs = trace.render_secs;
        self.tel_deliver_secs = trace.deliver_secs;
        self.tel_io_secs += trace.io_secs;

        self.render_handler = RenderHandler::Handoff;
        self.render_renderer = Some(format!("handoff/tier{target_tier}"));
    }

    //  Pipeline entry

    /// Run the full pipeline and return `(result, trace, after)`.
    ///
    /// `after` holds deferred cache-write futures.  Callers must drain it
    /// after the HTTP response is sent:
    /// - Native: `after.drain_spawn()` fires all tasks on the tokio thread pool.
    /// - Workers: iterate `after.drain()` and pass each to `ctx.wait_until()`.
    pub async fn run(self) -> (ThumbResult, ThumbTrace, AfterResponse)
    where
        S: Send + 'static,
    {
        self.run_with_progress(None).await
    }

    /// Run the cache-resolution phase: client freshness check, pre-connect
    /// KV check, HTTP connect, cache-key derivation, post-connect KV check.
    ///
    /// Returns `true` if the cook resolved to a terminal state (cache hit,
    /// 304 Not Modified, connect error, etc.) and `finish()` should be called.
    /// Returns `false` if the pipeline should continue - the connection is
    /// open, headers are populated, and `media.kind` is not yet set.
    ///
    /// Sets `cache_resolved = true` so `run_with_progress` skips this phase
    /// when called afterwards.  The caller (cloud wrapper) can interpose
    /// rate-limit checks between this call and `run_with_progress`.
    pub async fn resolve_cache(&mut self, _after: &mut AfterResponse, t0: web_time::Instant) -> bool
    where
        S: Send + 'static,
    {
        self.cache_resolved = true;

        //  Client-side freshness fast path
        if self.input.cache.as_ref().is_some_and(|h| h.is_fresh()) {
            self.status = CookStatus::Fresh;
            self.out_duration = t0.elapsed().as_secs_f64();
            return true;
        }

        //  Pre-connect cache check
        let mut pre_cached: Option<ThumbResult> = None;
        let mut pre_cache_backend: Option<String> = None;
        if !self.ctx_handoff && self.input.cache.is_none() {
            use crate::source::canonical_url;
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            use sha2::{Digest, Sha256};

            let identity = canonical_url(&self.input.url).unwrap_or_else(|| self.input.url.clone());
            let key_input = format!(
                "v{}:{}:{identity}",
                crate::TBR_CACHE_VERSION,
                self.ctx_customer_id.as_deref().unwrap_or("")
            );
            let url_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(key_input.as_bytes()));
            let account_id = self.ctx_customer_id.as_deref().unwrap_or("_");
            let pre_key = format!("{account_id}/{url_hash}");

            if let Some((cached, backend_name)) = self.runtime.cache.check(&pre_key).await {
                let decoded = cached
                    .media
                    .as_ref()
                    .filter(|m| !m.cache.is_empty())
                    .map(|m| m.cache.as_str())
                    .and_then(CacheHints::decode);
                if decoded.as_ref().is_some_and(|h| h.is_fresh()) {
                    self.http_close().await;
                    if let Some(ref media) = cached.media {
                        self.out_thumbnail = media.thumbnail.clone();
                        self.media.mime = Some(media.mime.clone());
                        self.media.file_size = Some(media.file_size);
                        self.media.kind = Some(media.kind);
                        self.media.extension = Some(crate::pipeline::canonical_extension(&media.extension));
                        self.media.properties = Some(media.properties.clone());
                    }
                    self.out_message = cached.message.unwrap_or_default();
                    self.out_placeholder = cached.media.as_ref().and_then(|m| {
                        if m.placeholder.is_empty() { None } else { Some(m.placeholder.clone()) }
                    });
                    self.out_download_bytes = cached.download_size;
                    self.src.cache_hints = decoded;
                    self.cache_hit = Some(backend_name.to_string());
                    self.status = CookStatus::Complete;
                    self.out_duration = t0.elapsed().as_secs_f64();
                    return true;
                }

                self.input.cache = decoded;
                pre_cached = Some(cached);
                pre_cache_backend = Some(backend_name.to_string());
            }
        }

        //  connect
        let t_step = web_time::Instant::now();
        pipeline::connect(self).await;
        self.tel_connect_secs = t_step.elapsed().as_secs_f64();
        if !self.status.is_processing() {
            if self.status == CookStatus::Fresh {
                if let Some(cached) = pre_cached {
                    if let Some(ref media) = cached.media {
                        self.out_thumbnail = media.thumbnail.clone();
                        self.media.mime = Some(media.mime.clone());
                        self.media.file_size = Some(media.file_size);
                        self.media.kind = Some(media.kind);
                        self.media.extension = Some(crate::pipeline::canonical_extension(&media.extension));
                        self.media.properties = Some(media.properties.clone());
                    }
                    self.out_message = cached.message.unwrap_or_default();
                    self.out_placeholder = cached.media.as_ref().and_then(|m| {
                        if m.placeholder.is_empty() { None } else { Some(m.placeholder.clone()) }
                    });
                    self.out_download_bytes = cached.download_size;
                    self.src.cache_hints = cached
                        .media
                        .as_ref()
                        .filter(|m| !m.cache.is_empty())
                        .map(|m| m.cache.as_str())
                        .and_then(CacheHints::decode);
                    self.cache_hit = pre_cache_backend.clone();
                }
            }
            self.stamp_download_bytes();
            self.out_duration = t0.elapsed().as_secs_f64();
            return true;
        }

        //  cache key derivation
        // Key on the canonical URL only.  Content-identity headers (ETag,
        // Content-MD5, x-amz-checksum-sha256) are not used here — they are
        // freshness validators stored *inside* the cached record and checked
        // on retrieval via CacheHints.  Using them in the key would cause
        // cross-URL contamination when multiple resources share the same
        // ETag (e.g. fake-etag demo files, CDN hour-bucketed responses).
        {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            use sha2::{Digest, Sha256};

            let identity = self
                .src
                .canonical_url
                .clone()
                .or_else(|| self.src.final_url.clone())
                .or_else(|| crate::source::canonical_url(&self.input.url))
                .unwrap_or_else(|| self.input.url.clone());

            let key_input = format!(
                "v{}:{}:{identity}",
                crate::TBR_CACHE_VERSION,
                self.ctx_customer_id.as_deref().unwrap_or("")
            );
            let url_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(key_input.as_bytes()));
            let account_id = self.ctx_customer_id.as_deref().unwrap_or("_");
            self.src.cache_key = Some(format!("{account_id}/{url_hash}"));
            self.src.cache_key_source = Some("url".to_string());
        }

        //  cache check
        if !self.ctx_handoff
            && let Some(ref key) = self.src.cache_key.clone()
            && let Some((cached, backend_name)) = self.runtime.cache.check(key).await
        {
            self.http_close().await;
            if let Some(ref media) = cached.media {
                self.out_thumbnail = media.thumbnail.clone();
                self.media.mime = Some(media.mime.clone());
                self.media.file_size = Some(media.file_size);
                self.media.kind = Some(media.kind);
                self.media.extension = Some(media.extension.clone());
                self.media.properties = Some(media.properties.clone());
            }
            self.out_message = cached.message.unwrap_or_default();
            self.out_placeholder = cached
                .media
                .as_ref()
                .and_then(|m| if m.placeholder.is_empty() { None } else { Some(m.placeholder.clone()) });
            self.out_download_bytes = cached.download_size;
            self.src.cache_hints = cached
                .media
                .as_ref()
                .filter(|m| !m.cache.is_empty())
                .map(|m| m.cache.as_str())
                .and_then(CacheHints::decode);
            self.cache_hit = Some(backend_name.to_string());
            self.status = CookStatus::Complete;
            self.out_duration = t0.elapsed().as_secs_f64();
            return true;
        }

        // Cache did not resolve.  Connection is open, headers are set.
        // Caller should check rate limits, then call run_with_progress to
        // continue with inspect / shortcut / render.
        false
    }

    /// Run the full pipeline and optionally emit intermediate progress snapshots.
    pub async fn run_with_progress(
        mut self,
        #[allow(unused_mut, unused_variables)] mut on_progress: Option<Box<dyn FnMut(ThumbResult) + Send>>,
    ) -> (ThumbResult, ThumbTrace, AfterResponse)
    where
        S: Send + 'static,
    {
        let mut after = AfterResponse::new();
        let t0 = web_time::Instant::now();

        //  Cache-resolution phase
        // Skip when resolve_cache() already ran externally (cloud wrapper
        // calls it to interpose rate-limit checks between cache and render).
        if !self.cache_resolved {
            if self.resolve_cache(&mut after, t0).await {
                return self.finish(after);
            }
        } else {
            // resolve_cache already ran; the cook is either at Processing
            // (continue) or a terminal state.
            if !self.status.is_processing() {
                return self.finish(after);
            }
        }

        //  Below this point: cache did not resolve.  The connection is open,
        // headers are populated.  Proceed with inspect / shortcut / render.

        if !self.ctx_handoff {
            //  inspect
            let t_step = web_time::Instant::now();
            pipeline::inspect(&mut self).await;
            self.tel_inspect_secs = t_step.elapsed().as_secs_f64();
            if !self.status.is_processing() {
                self.stamp_download_bytes();
                self.out_duration = t0.elapsed().as_secs_f64();
                return self.finish(after);
            }
        }

        //  shortcut
        // Runs in both direct and handoff modes.  On a handoff, media
        // kind/extension are already set from the tier-1 inspect, so the
        // shortcut has all the context it needs.  Tier-2's higher
        // ShortcutLimits (unlimited progressive pixels, 200 KiB small-file
        // threshold, 2 MiB ZIP tail) cover cases tier-1 had to skip.
        {
            let t_step = web_time::Instant::now();
            pipeline::shortcut(&mut self).await;
            self.tel_shortcut_secs = t_step.elapsed().as_secs_f64();
            self.stamp_download_bytes();

            if self.status == CookStatus::Complete {
                self.out_duration = t0.elapsed().as_secs_f64();
                if !self.ctx_handoff
                    && let Some(ref key) = self.src.cache_key.clone()
                {
                    // Override freshness so the client knows to retry
                    // after the quota window ends.
                    if self.render_disabled {
                        if let Some(ttl) = self.render_disabled_ttl_secs {
                            self.src.cache_hints = Some(CacheHints::expiring_in(ttl));
                        }
                    }
                    let result = self.to_result();
                    let cost = render_cost_from_secs(self.out_duration);
                    let now = web_time::SystemTime::now()
                        .duration_since(web_time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let expires = if let Some(ttl) = self.render_disabled_ttl_secs {
                        now + ttl
                    } else {
                        self.cache_expires_at()
                    };
                    self.runtime.cache.store(key, &result, cost, expires, &mut after);
                }
                return self.finish(after);
            }

            // Intermediate progress: only emit when there's a real async gap
            // (handoff to higher tier).  In-process renderer results are immediate
            // - success or placeholder - so an intermediate just adds noise.
        }

        //  deliver (when shortcut decoded an image)
        if self.render_image.is_some() {
            let t_step = web_time::Instant::now();
            pipeline::deliver(&mut self).await;
            self.tel_deliver_secs = t_step.elapsed().as_secs_f64();
            self.out_duration = t0.elapsed().as_secs_f64();
            if !self.ctx_handoff
                && self.status == CookStatus::Complete
                && let Some(ref key) = self.src.cache_key.clone()
            {
                let result = self.to_result();
                let cost = render_cost_from_secs(self.out_duration);
                let expires = self.cache_expires_at();
                self.runtime.cache.store(key, &result, cost, expires, &mut after);
            }
            return self.finish(after);
        }

        //  render-disabled gate - over quota: no handoff or render
        if self.render_disabled {
            // Shortcut didn't produce a result and deliver didn't fire.
            // Produce a placeholder based on inspect results.  finish()
            // selects the appropriate placeholder JPEG by FileKind.
            self.status = CookStatus::Failed;
            self.out_message = "render quota reached".to_string();
            self.out_duration = t0.elapsed().as_secs_f64();
            return self.finish(after);
        }

        //  handoff to higher tier
        // Check for a registered in-process renderer *before* closing the
        // connection so the renderer can stream from the live HttpBuffer.
        #[cfg(feature = "native")]
        if let Some(renderer) = self.runtime.renderer.clone() {
            // Emit intermediate progress before the async render gap.
            if !self.ctx_handoff
                && let Some(ref mut progress) = on_progress
            {
                progress(self.to_progress_result(t0.elapsed().as_secs_f64()));
            }

            // Coerce &mut ThumbCook<S> → &mut dyn RenderCook.  Valid because
            // impl<S: HttpStream + Send + 'static> RenderCook for ThumbCook<S>
            // and run() requires S: Send + 'static.
            let t_render = web_time::Instant::now();
            if renderer.render(&mut self as &mut dyn RenderCook).await {
                // Renderer claimed the format.  On success render_image is set
                // and deliver produces the thumbnail.  On failure the renderer
                // called fail_cook() and render_image is None; status != Complete.
                self.tel_decode_secs = t_render.elapsed().as_secs_f64();
                self.render_handler =
                    if self.ctx_handoff { RenderHandler::Handoff } else { RenderHandler::Builtin };

                // Use the actual bytes consumed by the renderer when available
                // (reported via RenderCook::set_bytes_consumed); fall back to
                // file_size for formats that don't report partial reads (e.g.
                // image-crate paths that drain the full stream).
                if let Some(bytes) = self.render_bytes_consumed {
                    self.out_download_bytes = bytes;
                } else if let Some(sz) = self.media.file_size {
                    self.out_download_bytes = sz;
                }

                if self.render_image.is_some() {
                    let t_step = web_time::Instant::now();
                    pipeline::deliver(&mut self).await;
                    self.tel_deliver_secs = t_step.elapsed().as_secs_f64();
                    self.out_duration = t0.elapsed().as_secs_f64();
                    if !self.ctx_handoff
                        && self.status == CookStatus::Complete
                        && let Some(ref key) = self.src.cache_key.clone()
                    {
                        let result = self.to_result();
                        let cost = render_cost_from_secs(self.out_duration);
                        let expires = self.cache_expires_at();
                        self.runtime.cache.store(key, &result, cost, expires, &mut after);
                    }
                }
                return self.finish(after);
            }
            // Renderer returned false (format not recognised).
            // Contract: the renderer did not call take_reader(), so the
            // connection is still open.  Fall through to out-of-process
            // handoff - a higher tier may still be able to handle this.
            // Intermediate already emitted above; don't re-emit in handoff chain.
            self.placeholder_source = Some(ResultSource::Fallback);
            on_progress = None;
        }

        //  handoff fallback chain
        // Build the handoff payload once (closes the HTTP connection), then
        // try tiers in ascending order: start with the routing recommendation
        // and escalate upward on failure.  Tier 2 never falls back on its own;
        // tier 1 drives the entire chain.
        {
            let payload = self.take_handoff().await;
            let local_bytes = self.http_bytes_fetched();
            let ext = self.media.extension.clone().unwrap_or_default();
            let kind = self.media.kind;

            let routed_tier = kind.map(|k| crate::dispatch::route(k, Some(&ext)).tier).unwrap_or(2);

            // Escalate tier-1 images to tier 2 when tier 2 is available.
            let start_tier = if routed_tier == 1
                && kind == Some(FileKind::Image)
                && self.runtime.handoff_tier2.url.is_some()
            {
                2
            } else {
                routed_tier
            };

            // Build the ordered list of tiers to try.
            let mut tried_any = false;
            let mut first_attempt = true;
            for attempt_tier in start_tier..=3 {
                if attempt_tier == 1 {
                    continue; // tier 1 already tried locally
                }

                // Tier 3 gate: check the dynamic availability registry.
                // External tier 3 servers are assumed full-capability.
                if attempt_tier == 3 && !crate::dispatch::tier3_can_handle(&ext) {
                    continue;
                }

                let target = match attempt_tier {
                    2 => self.runtime.handoff_tier2.clone(),
                    3 => self.runtime.handoff_tier3.clone(),
                    _ => continue,
                };
                let Some(url) = target.url.as_deref() else {
                    continue;
                };
                tried_any = true;

                // Emit intermediate progress before the async handoff gap.
                if !self.ctx_handoff
                    && let Some(ref mut progress) = on_progress
                {
                    progress(self.to_progress_result(t0.elapsed().as_secs_f64()));
                }

                let outcome: Result<HandoffResponse, String> = if first_attempt {
                    first_attempt = false;
                    //  Single-flight dedup (first attempt only)
                    let hkey = self
                        .src
                        .cache_key
                        .clone()
                        .or_else(|| self.src.canonical_url.clone())
                        .unwrap_or_else(|| self.input.url.clone());
                    match self.runtime.handoff_inflight.try_lead(&hkey) {
                        Some(rx) => {
                            // Joiner: an in-flight handoff exists for this key.
                            match rx.await {
                                Ok(shared) => match &*shared {
                                    Ok(remote) => Ok(remote.clone()),
                                    Err(e) => Err(e.clone()),
                                },
                                Err(_) => Err("handoff was cancelled before completing".into()),
                            }
                        }
                        None => {
                            // Leader: perform the handoff, wake joiners.
                            let result = crate::handoff::post_handoff(url, &target.headers, &payload).await;
                            let shared = Arc::new(result);
                            self.runtime.handoff_inflight.complete(&hkey, Arc::clone(&shared));
                            match &*shared {
                                Ok(remote) => Ok(remote.clone()),
                                Err(e) => Err(e.clone()),
                            }
                        }
                    }
                } else {
                    //  Fallback attempt (no dedup)
                    crate::handoff::post_handoff(url, &target.headers, &payload).await
                };

                match outcome {
                    Ok(ref remote) if remote.result.status == ResultStatus::Success => {
                        self.apply_handoff_response(remote, attempt_tier, local_bytes);
                        if self.status == CookStatus::Complete {
                            self.out_duration = t0.elapsed().as_secs_f64();
                            return self.finish(after);
                        }
                        // Remote tier returned a non-OK status - escalate.
                        self.out_message.clear();
                    }
                    Ok(ref remote) => {
                        self.out_message = remote.result.message.clone().unwrap_or_else(|| {
                            format!("tier {attempt_tier} returned status {:?}", remote.result.status)
                        });
                    }
                    Err(e) => {
                        self.out_message = format!("handoff to tier {attempt_tier} failed: {e}");
                    }
                }
            }

            if tried_any {
                self.status = CookStatus::Failed;
                self.out_duration = t0.elapsed().as_secs_f64();
                return self.finish(after);
            }

            self.status = CookStatus::Complete;
            self.placeholder_source = Some(ResultSource::Placeholder);
            self.out_message = "no higher-tier renderer is configured".to_string();
            self.out_duration = t0.elapsed().as_secs_f64();
            self.finish(after)
        }
    }

    /// Compute the cache expiry timestamp for the current cook.
    ///
    /// Uses the upstream `CacheHints::expires_at` if available, capped by
    /// `runtime.cache_max_ttl_secs`.  Falls back to a default TTL when the
    /// upstream provides no freshness window.
    fn cache_expires_at(&self) -> u64 {
        let now = web_time::SystemTime::now()
            .duration_since(web_time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let default_expiry = now + self.runtime.cache_default_ttl_secs;
        let max_expiry = now + self.runtime.cache_max_ttl_secs;

        self.src
            .cache_hints
            .as_ref()
            .and_then(|h| h.expires_at)
            .unwrap_or(default_expiry)
            .min(max_expiry)
    }

    fn finish(mut self, mut after: AfterResponse) -> (ThumbResult, ThumbTrace, AfterResponse) {
        // Final snapshot of I/O counters - safe to call multiple times since
        // stamp_download_bytes is idempotent on the bytes field.
        self.stamp_download_bytes();

        // Always return a thumbnail.  If the pipeline didn't produce one,
        // fill in the appropriate placeholder JPEG - except for NotModified,
        // where an empty thumbnail tells the caller "use your cached copy".
        if self.out_thumbnail.is_empty() && self.status != CookStatus::Fresh {
            if let Some(kind) = self.media.kind {
                // Kind was identified: use the kind-specific placeholder.
                // If the pipeline failed at the render step (e.g. unsupported
                // codec, EXR, SVG) promote the status to Placeholder - we know
                // what the file is, we just can't render it yet.
                let tried_render = self.status == CookStatus::Failed;
                if tried_render {
                    self.status = CookStatus::Complete;
                    self.placeholder_source = Some(ResultSource::Fallback);
                }
                let slug = kind_slug(kind);
                self.out_thumbnail = crate::assets::placeholder_for_kind(kind).to_vec();
                self.out_placeholder = Some(slug.to_string());

                // Inject a cache expiry so clients know when to retry.
                // - No renderer for this format → 1 day.
                // - Tried and fell back → upstream cache with 1 h floor.
                let hints = self.src.cache_hints.take();
                self.src.cache_hints = if tried_render {
                    Some(hints.unwrap_or_default().with_min_expiry(3600))
                } else if let Some(ttl) = self.render_disabled_ttl_secs {
                    Some(CacheHints::expiring_in(ttl))
                } else {
                    Some(CacheHints::expiring_in(86400))
                };
            } else {
                // Kind never determined (network error, bad URL, early abort) -
                // use the FAILED icon only for genuine infrastructure errors.
                let (bytes, label): (&[u8], &str) = if self.status == CookStatus::Failed
                    || self.out_placeholder.as_deref() == Some("error")
                    || self.out_placeholder.as_deref() == Some("failed")
                {
                    (crate::assets::placeholders::FAILED, "failed")
                } else {
                    (crate::assets::placeholders::UNKNOWN, "unknown")
                };
                self.out_thumbnail = bytes.to_vec();
                self.out_placeholder = Some(label.to_string());
            }
        }

        let result = self.to_result();
        let trace = self.to_trace();
        self.runtime.trace.record(trace.clone(), &mut after);
        (result, trace, after)
    }
}

/// Map a [`FileKind`] to its stable lowercase ASCII slug used in placeholder labels.
fn kind_slug(kind: crate::media::FileKind) -> &'static str {
    use crate::media::FileKind::*;
    match kind {
        Image => "image",
        Video => "video",
        Audio => "audio",
        Vector => "vector",
        Document => "document",
        Geometry => "geometry",
        Archive => "archive",
        Text => "text",
        Binary => "binary",
        Unknown => "unknown",
    }
}

fn cook_status_from_job(status: ResultStatus) -> CookStatus {
    match status {
        ResultStatus::Success => CookStatus::Complete,
        ResultStatus::Failed => CookStatus::Failed,
        ResultStatus::Overloaded => CookStatus::Overloaded,
        ResultStatus::Intermediate => CookStatus::Intermediate,
    }
}

//  RenderCook impl
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
    fn peek_bytes(&self, len: usize) -> Option<Vec<u8>> {
        let page = self.http_buf.as_ref()?.peek_page0()?;
        if page.len() >= len { Some(page[..len].to_vec()) } else { None }
    }
    fn is_handoff(&self) -> bool {
        self.ctx_handoff
    }
    fn set_render_image(&mut self, img: image::DynamicImage) {
        self.render_image = Some(img);
    }
    fn has_render_image(&self) -> bool {
        self.render_image.is_some()
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
        // Merge into existing properties so inspect/shortcut dimensions survive.
        let mut merged = self.media.properties.take().unwrap_or_default();
        if let (Some(existing), Some(new)) = (merged.as_object_mut(), props.as_object()) {
            for (k, v) in new {
                existing.insert(k.clone(), v.clone());
            }
        } else {
            merged = props;
        }
        self.media.properties = Some(merged);
    }
    fn fail_cook(&mut self, msg: &str) {
        self.fail(msg);
    }
    fn set_bytes_consumed(&mut self, n: u64) {
        self.render_bytes_consumed = Some(n);
    }
}
