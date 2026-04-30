//! Tier 1 library — core types and pipeline.
//!
//! # Build modes
//!
//! `tier1` has two build modes controlled by the `native` Cargo feature:
//!
//! | Mode          | Feature flag   | Target             | Notes                        |
//! |---------------|----------------|--------------------|------------------------------|
//! | Native server | `native` (default) | `x86_64-*` etc. | tokio + axum + reqwest     |
//! | Workers WASM  | no `native`    | `wasm32-unknown-unknown` | downstream crate adds workers-rs |
//!
//! All modules in the `pub` surface of this crate must compile on
//! `wasm32-unknown-unknown`.  OS/thread-dependent code lives behind
//! `#[cfg(feature = "native")]`.
//!
//! # Abbreviation convention
//!
//! The project uses `tbr` as the short form of "thumbrella" in identifiers
//! (e.g. `TBR_VERSION`).  Public API types follow the family naming scheme:
//! - `Call*` — outer HTTP envelope types (`CallRequest`, `CallResponse`, …)
//! - `Thumb*` — per-item types (`ThumbInput`, `ThumbResult`, `ThumbTrace`, `ThumbCook`, `ThumbHandoff`, …)

// ── Core modules (always compiled) ───────────────────────────────────────────
pub mod after;
pub mod cache;
pub mod cook;
pub mod dispatch;
pub mod handoff;
pub mod http_buf;
pub mod media;
pub mod pipeline;
pub mod spec;
pub mod request;
pub mod result;
pub mod source;
pub mod tracelog;

// ── Native-only modules ───────────────────────────────────────────────────────

#[cfg(feature = "native")]
pub mod config;

#[cfg(feature = "native")]
pub mod diag;

#[cfg(feature = "native")]
pub mod routes;

#[cfg(feature = "native")]
pub mod startup;

#[cfg(feature = "native")]
pub mod cli;

#[cfg(feature = "native")]
pub mod renderer;

// ── Convenience re-exports ────────────────────────────────────────────────────

pub use cook::{CallerContext, CookStatus, InputSpec, MediaInfo, Runtime, SourceIdentity, ThumbCook as ThumbCookGeneric};
#[cfg(feature = "native")]
pub use renderer::{InProcessRenderer, RenderCook, RenderOutput, SharedRenderer, apply_render_output, with_renderer};
pub use spec::ShortcutLimits;
pub use dispatch::{ThumbRoute, route};
pub use handoff::ThumbHandoff;
pub use media::{FileKind, Strategy};
pub use request::{CallRequest, ThumbInput, ThumbObject};
pub use result::{CacheOutcome, CallRecord, CallResponse, JobStatus, RenderHandler, ThumbResult, ThumbTrace};
pub use source::{SourceRef, canonical_url, conditional_headers, etag_from_headers};

/// Concrete `ThumbCook` type for native server builds.
#[cfg(feature = "native")]
pub type ThumbCook = cook::ThumbCook<http_buf::PlatformStream>;

/// Combined `Read + Seek` supertrait.  Use `Box<dyn ReadSeek + Send>` where a
/// type-erased seekable reader is needed (e.g. libav AVIOContext opaque).
pub use http_buf::ReadSeek;

/// Synchronous `Read + Seek` adapter over the live HTTP buffer.
///
/// Use this to pass an [`http_buf::HttpBuffer`] directly into libav's
/// `AVIOContext` callbacks (which run on the blocking thread inside
/// `spawn_blocking`).  Each `read` call is bridged back to the async
/// paged cache via the tokio handle captured at construction.
#[cfg(feature = "native")]
pub use http_buf::SyncHttpReader;

// ── Crate-level constants ─────────────────────────────────────────────────────

/// Semantic version of this crate.
pub const TBR_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cache format version.  Increment this to invalidate all cached results
/// globally — e.g. after a breaking change to `ThumbResult`, thumbnail
/// dimensions, or image quality settings.
///
/// Baked into the SHA-256 key input so old entries become unreachable without
/// any schema migration or explicit flush.
pub const TBR_CACHE_VERSION: u32 = 1;

// ── Runtime builder helpers ───────────────────────────────────────────────────

/// Replace the [`ShortcutLimits`] on an existing [`Runtime`].
///
/// Call this in the tier 2 startup hook to relax the conservative tier 1
/// defaults:
///
/// ```ignore
/// tier1::cli::run_with_hook(|rt| async move {
///     let rt = tier1::with_renderer(rt, tier2::Tier2Renderer::shared());
///     tier1::with_shortcut_limits(rt, tier1::ShortcutLimits::TIER2)
/// }).await;
/// ```
pub fn with_shortcut_limits(runtime: std::sync::Arc<Runtime>, limits: ShortcutLimits) -> std::sync::Arc<Runtime> {
    let mut r = std::sync::Arc::try_unwrap(runtime)
        .unwrap_or_else(|arc| (*arc).clone());
    r.shortcut_limits = limits;
    std::sync::Arc::new(r)
}
