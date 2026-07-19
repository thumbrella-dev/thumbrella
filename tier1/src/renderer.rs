//! In-process render extension point.
//!
//! Tier 1 owns the full pipeline (connect → inspect → shortcut → **render** →
//! deliver → cache → trace).  The render step is the only part it cannot
//! perform entirely in-process on its own - formats like video, HEIC, EXR,
//! SVG, and documents require native libraries beyond what tier 1 links.
//!
//! # One cook, all the way through
//!
//! There is exactly one [`crate::cook::ThumbCook`] per request.  The renderer
//! receives it as `&mut dyn RenderCook` - a thin trait-object view that erases
//! the `S` (HttpStream) type parameter and exposes only what the renderer
//! needs:
//! - **Read** - `take_reader()` moves the buffer out as a
//!   `Box<dyn ReadSeek + Send>` while preserving random-access page-cache
//!   semantics.  libav's `AVIOContext` callbacks call into this synchronously,
//!   driving HTTP reads on-demand without buffering the whole file.
//! - **Metadata** - `media_kind()`, `media_extension()`, `content_length()`,
//!   `input_url()`.
//! - **Write-back** - `set_render_image()`, `set_render_renderer()`, etc.
//!   The renderer writes results directly into the cook's fields; tier 1 then
//!   runs its own `deliver` step as normal.
//!
//! # Logging ownership
//!
//! The entry-point tier owns the trace record.  When tier 2 is invoked
//! in-process from a tier 1 cook, tier 1 writes the trace as usual with
//! `job_tier = 1`.  When tier 2 runs standalone its trace uses `job_tier = 2`.
//!
//! # Contract
//!
//! Implementations must:
//! - Set `cook.set_render_image(img)` on success.
//! - Return `true` when the format was claimed (whether decode succeeded
//!   or not - call `cook.fail_cook(msg)` on unrecoverable errors).
//! - Return `false` to signal "not my format".

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use image::DynamicImage;

use crate::cook::Runtime;
use crate::http_buf::ReadSeek;
use crate::media::FileKind;

//  RenderCook - type-erased view of ThumbCook for renderer impls

/// Trait-object interface passed to [`InProcessRenderer::render`].
///
/// Erases the `S: HttpStream` type parameter so the renderer trait stays
/// object-safe and renderer code has no compile-time dependency on the HTTP
/// backend.  See the [module-level docs](self) for the full contract.
pub trait RenderCook: Send {
    /// Detected media kind.
    fn media_kind(&self) -> Option<FileKind>;
    /// Canonical file extension (e.g. `"jpeg"`, `"heic"`, `"mp4"`).
    fn media_extension(&self) -> Option<&str>;
    /// Source URL.
    fn input_url(&self) -> &str;
    /// `Content-Length` reported by the server, if available.
    fn content_length(&self) -> Option<u64>;

    /// Enter streaming mode on the buffer and move it out as a sync reader.
    ///
    /// Streaming mode means the `HttpBuffer` will not add new pages to its
    /// cache going forward, keeping memory bounded for large files.  Already
    /// cached pages (from the inspect / shortcut phase) remain readable.
    ///
    /// The returned reader drives HTTP I/O on-demand: each `read` call blocks
    /// the calling thread (inside `spawn_blocking`) and bridges back to the
    /// tokio runtime via the handle captured at reader construction.
    ///
    /// Returns `None` if no connection is currently open.
    fn take_reader(&mut self) -> Option<Box<dyn ReadSeek + Send>>;

    /// Peek at the first `len` bytes of the source without consuming the
    /// reader.  Returns bytes from the already-cached first page (populated
    /// during inspect).  Returns `None` if fewer than `len` bytes are
    /// available or no connection is open.
    ///
    /// This is safe to call before `take_reader()` - the reader remains
    /// intact for the chosen backend.
    fn peek_bytes(&self, len: usize) -> Option<Vec<u8>>;

    /// Whether this cook is serving a handoff from a lower tier.  When
    /// true, the renderer should skip lower-tier processing (shortcut,
    /// tier2 paths) and go directly to this tier's backends.
    fn is_handoff(&self) -> bool;

    /// Store the decoded pixel buffer.  Tier 1's `deliver` step will
    /// resize and JPEG-encode this image after `render` returns.
    fn set_render_image(&mut self, img: DynamicImage);
    /// Whether a render image was stored (i.e. the format was decoded
    /// successfully).  Higher tiers use this to decide whether to try
    /// a fallback renderer after a lower tier claims the format.
    fn has_render_image(&self) -> bool {
        false
    }
    /// Low-level renderer label for the trace (e.g. `"ffmpeg"`, `"image_crate"`).
    fn set_render_renderer(&mut self, label: String);
    /// Codec or container detail for the trace (e.g. `"hevc"`, `"av1"`).
    fn set_render_codec(&mut self, codec: String);
    /// Video seek offset in seconds.
    fn set_render_video_seek_secs(&mut self, secs: f64);
    /// Source-level properties (width, height, depth, …) written to the
    /// trace and returned to the client.
    fn set_media_properties(&mut self, props: serde_json::Value);
    /// Mark the cook as failed with a human-readable message.
    fn fail_cook(&mut self, msg: &str);
    /// Report the actual bytes consumed by the renderer (e.g. AVIO bytes from
    /// libav).  When set, this value is preferred over the `file_size` fallback
    /// in the download counter, enabling accurate reporting for formats that
    /// use partial reads (e.g. HEIC/AVIF with probe_limit).
    fn set_bytes_consumed(&mut self, n: u64);
}

// RenderCook is implemented for ThumbCook in cook.rs (needs private field access).

//  RenderOutput - convenience struct for internal decode pipelines

/// Intermediate result from a decode function (image crate, libav, etc.).
///
/// Renderer implementations return this from their internal decode helpers,
/// then apply it to the cook via [`apply_render_output`].
pub struct RenderOutput {
    pub image: DynamicImage,
    pub renderer: Option<String>,
    pub codec: Option<String>,
    pub video_seek_secs: Option<f64>,
    pub properties: Option<serde_json::Value>,
}

/// Apply a `RenderOutput` to a `dyn RenderCook` in one call.
pub fn apply_render_output(cook: &mut dyn RenderCook, out: RenderOutput) {
    cook.set_render_image(out.image);
    if let Some(r) = out.renderer {
        cook.set_render_renderer(r);
    }
    if let Some(c) = out.codec {
        cook.set_render_codec(c);
    }
    if let Some(s) = out.video_seek_secs {
        cook.set_render_video_seek_secs(s);
    }
    if let Some(p) = out.properties {
        cook.set_media_properties(p);
    }
}

//  InProcessRenderer

/// In-process render extension installed at startup by a higher tier.
///
/// See module-level docs for the full contract.
pub trait InProcessRenderer: Send + Sync + 'static {
    /// Attempt to render the item in `cook`.
    ///
    /// Returns `true` when the format was claimed (regardless of success -
    /// call `cook.fail_cook(msg)` on decode errors).
    /// Returns `false` to signal "not my format".
    fn render<'a>(&'a self, cook: &'a mut dyn RenderCook) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

//  Type alias

/// Shared handle to an in-process renderer installed at startup.
pub type SharedRenderer = Arc<dyn InProcessRenderer>;

//  Runtime builder

/// Attach an in-process renderer to an existing [`Runtime`].
pub fn with_renderer(runtime: Arc<Runtime>, renderer: SharedRenderer) -> Arc<Runtime> {
    let mut r = Arc::try_unwrap(runtime).unwrap_or_else(|arc| (*arc).clone());
    r.renderer = Some(renderer);
    Arc::new(r)
}
