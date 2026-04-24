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
//! (e.g. `TbrError`, `TBR_VERSION`).  Generic type names (`BatchRequest`,
//! `ItemResult`, …) carry no project prefix.

// ── Core modules (always compiled) ───────────────────────────────────────────

pub mod media;
pub mod pipeline;
pub mod profile;
pub mod request;
pub mod result;
pub mod source;
pub mod http_buf;

// ── Native-only modules ───────────────────────────────────────────────────────

#[cfg(feature = "native")]
pub mod config;

#[cfg(feature = "native")]
pub mod routes;

// ── Convenience re-exports ────────────────────────────────────────────────────

pub use media::FileKind;
pub use profile::ThumbnailConfig;
pub use request::{BatchOptions, BatchRequest, CacheMode, ItemInput, ItemObject, ItemRequest, RequestedOps};
pub use result::{BatchResponse, ItemResponse, JobStatus, RequestRecord, ServerInfo};
pub use source::{SourceMetadata, SourceRef, SourceValidator, canonical_url};

// ── Crate-level constants ─────────────────────────────────────────────────────

/// Semantic version of this crate.
pub const TBR_VERSION: &str = env!("CARGO_PKG_VERSION");
