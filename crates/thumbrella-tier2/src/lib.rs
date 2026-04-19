//! Tier 2 pipeline surface.
//!
//! Tier 2 extends Tier 1 with additional source handlers (for example libav)
//! while preserving Tier 1 behavior via fallback paths.

pub mod pipeline;
pub mod routes;

pub use thumbrella_tier1::*;
