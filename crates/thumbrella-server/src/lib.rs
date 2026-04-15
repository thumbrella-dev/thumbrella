//! Shared library surface for the server crate.
//!
//! The HTTP binary and dev CLI binary both import from here so they use the
//! same processing pipeline implementation.

pub mod pipeline;
pub mod routes;
