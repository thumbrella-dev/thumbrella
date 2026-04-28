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

// ── Convenience re-exports ────────────────────────────────────────────────────

pub use cook::{CallerContext, CookStatus, InputSpec, MediaInfo, Runtime, SourceIdentity, ThumbCook as ThumbCookGeneric};
pub use dispatch::{ThumbRoute, route};
pub use handoff::ThumbHandoff;
pub use media::{FileKind, Strategy};
pub use request::{CallRequest, ThumbInput, ThumbObject};
pub use result::{CacheOutcome, CallRecord, CallResponse, JobStatus, RenderHandler, ThumbResult, ThumbTrace};
pub use source::{SourceRef, canonical_url, conditional_headers, etag_from_headers};

/// Concrete `ThumbCook` type for native server builds.
#[cfg(feature = "native")]
pub type ThumbCook = cook::ThumbCook<http_buf::PlatformStream>;

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
