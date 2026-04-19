//! Shared library surface for the server crate.
//!
//! The HTTP binary and dev CLI binary both import from here so they use the
//! same processing pipeline implementation.

pub mod request;
pub mod result;
pub mod profile;
pub mod source;
pub mod pipeline;
pub mod paged_io;
pub mod http_source;
pub mod routes;

pub use request::*;
pub use result::*;
pub use profile::*;
pub use source::*;
