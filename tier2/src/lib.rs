//! Tier 2 library.
//!
//! Re-exports everything from `tier1` and adds extended decode handlers
//! (video, HEIC, EXR, …) on top.

pub use tier1::*;

pub mod avdecode;
pub mod renderer;
pub use renderer::Tier2Renderer;
