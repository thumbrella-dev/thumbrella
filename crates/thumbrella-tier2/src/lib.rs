//! Tier 2 pipeline surface.
//!
//! Tier 2 provides libav-based decode handlers (HEIC, video, AVIF, EXR,
//! attached picture).  All HTTP routing is handled by Tier 1; Tier 2 registers
//! itself via `thumbrella_tier1::dispatch::register_tier2` at startup.

pub mod pipeline;

pub use thumbrella_tier1::*;
