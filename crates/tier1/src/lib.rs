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
//! - `Thumb*` — per-item types (`ThumbInput`, `ThumbResponse`, `ThumbSpec`, `ThumbCook`, …)

// ── Core modules (always compiled) ───────────────────────────────────────────
pub mod after;pub mod cook;
pub mod dispatch;
pub mod http_buf;
pub mod media;
pub mod pipeline;
pub mod spec;
pub mod request;
pub mod result;
pub mod source;

// ── Native-only modules ───────────────────────────────────────────────────────

#[cfg(feature = "native")]
pub mod config;

#[cfg(feature = "native")]
pub mod diag;

#[cfg(feature = "native")]
pub mod routes;

#[cfg(feature = "native")]
pub mod startup;

// ── Convenience re-exports ────────────────────────────────────────────────────

pub use after::{AfterResponse, DeferredFuture};
pub use cook::{CallerContext, ThumbCook as ThumbCookGeneric, ThumbSpec, ThumbTrace};
pub use dispatch::{ThumbRoute, route};
pub use media::{FileKind, Strategy};
pub use spec::ThumbnailConfig;
pub use request::{CallRequest, ThumbInput, ThumbObject};
pub use result::{CacheOutcome, CallRecord, CallResponse, JobStatus, RenderHandler, ThumbResult};
pub use source::{SourceRef, canonical_url, conditional_headers, etag_from_headers};

/// Concrete `ThumbCook` type for native server builds.
///
/// Use this alias instead of `cook::ThumbCook<http_buf::PlatformStream>` at
/// call sites within native-only code.
#[cfg(feature = "native")]
pub type ThumbCook = cook::ThumbCook<http_buf::PlatformStream>;

// ── Crate-level constants ─────────────────────────────────────────────────────

/// Semantic version of this crate.
pub const TBR_VERSION: &str = env!("CARGO_PKG_VERSION");
