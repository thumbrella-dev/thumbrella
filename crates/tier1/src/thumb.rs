//! Pipeline state — `ThumbPipeline` and `MediaLog`.
//!
//! `ThumbPipeline<S>` is the single object that carries everything needed
//! to process one thumbnail from request through to response.  It owns:
//!
//! - The request fields directly (url, validator, ops).
//! - The accumulating `ThumbResponse` — filled in as each pipeline step
//!   runs.  When the pipeline finishes this is what goes to the wire and
//!   into the cache.  No translation step, no parallel set of field names.
//! - A `MediaLog` — internal telemetry that never leaves the server.
//! - Live resources (`http`, `render`, `thumb`) — replaced / dropped as
//!   each step completes.
//!
//! # Placeholder image types
//!
//! `RenderImage` and `ThumbnailImage` are stubs today.  They will become
//! real decoded / encoded image buffers once the render and deliver steps
//! are implemented.

use crate::http_buf::{HttpBuffer, HttpStream};
use crate::result::{JobStatus, ThumbResponse};
use crate::source::SourceRef;

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

// ── MediaLog ──────────────────────────────────────────────────────────────────

/// Internal per-item telemetry — the server's private record of work done.
///
/// Never sent to clients.  Matches the "Media Logged" schema section.
/// Populated incrementally as pipeline steps run; emitted to the log sink
/// when the pipeline finishes.
#[derive(Debug, Default)]
pub struct MediaLog {
    // ── Source identity ───────────────────────────────────────────────────────
    /// Canonical URL (query params / signing tokens stripped).
    pub canonical_url: Option<String>,
    /// SHA-256 of the canonical URL — used as the storage key.
    pub url_hash: Option<String>,

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
}

// ── ThumbPipeline ─────────────────────────────────────────────────────────────

/// Full processing state for one thumbnail request.
///
/// This is the only object the pipeline functions receive and return.
/// Request fields are owned directly; there is no separate `ThumbRequest`.
pub struct ThumbPipeline<S: HttpStream> {
    // ── Request fields (set at construction) ──────────────────────────────────
    /// Source to process.
    pub source: SourceRef,
    /// Caller's previously seen validator token for conditional fetches.
    pub validator: Option<String>,

    // ── Accumulating response (mutated by each step) ───────────────────────────
    /// The result that will be cached and sent to the client.
    /// Fields are `None` / zero until the relevant step runs.
    pub response: ThumbResponse,

    // ── Internal telemetry (mutated by each step) ─────────────────────────────
    pub log: MediaLog,

    // ── Live resources (replaced / dropped as steps complete) ─────────────────
    /// Open HTTP connection; present during connect→inspect→render, then closed.
    pub http: Option<HttpBuffer<S>>,
    /// Decoded pixel buffer; present during render→deliver, then dropped.
    pub render: Option<RenderImage>,
    /// Encoded thumbnail; present after deliver until response is emitted.
    pub thumb: Option<ThumbnailImage>,
}

impl<S: HttpStream> ThumbPipeline<S> {
    /// Create a new pipeline for a URL source.
    pub fn new(url: String, validator: Option<String>) -> Self {
        let url_clone = url.clone();
        Self {
            source: SourceRef::url(url),
            validator,
            response: ThumbResponse {
                url: url_clone,
                ..ThumbResponse::default()
            },
            log: MediaLog::default(),
            http: None,
            render: None,
            thumb: None,
        }
    }

    /// Convenience: mark the response as failed with a message.
    pub fn fail(&mut self, message: impl Into<String>) {
        self.response.status = JobStatus::Failed;
        self.response.message = message.into();
    }

    /// Take the accumulated response, consuming the pipeline.
    ///
    /// Called once at the end of processing to extract what goes to the wire.
    pub fn into_response(self) -> ThumbResponse {
        self.response
    }
}
