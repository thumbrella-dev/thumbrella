//! Core domain types shared across all Thumbrella tiers.
//!
//! Every tier speaks the same request/response language. Escalation from
//! Tier 1 → Tier 2 → Tier 3 is just forwarding the same `BatchRequest`.

pub mod request;
pub mod result;
pub mod profile;
pub mod source;

pub use request::*;
pub use result::*;
pub use profile::*;
pub use source::*;
