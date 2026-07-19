//! <div align="center">
//!
//! # Thumbrella
//!
//! **Fast thumbnail server for online media.**
//!
//! [Website](https://thumbrella.dev) · [Docs](https://thumbrella.dev/docs/) · [Client packages](https://thumbrella.dev/docs/client/)
//!
//! </div>
//!
//! ---
//!
//! ## About
//!
//! Thumbrella serves fast, cached thumbnails from over 100 formats:
//! photographs, video, documents, and 3D models. One command runs it locally
//! or in Docker.
//!
//! This crate is the **workspace root** it provides the top-level
//! documentation entry point for the entire codebase. The server is split
//! across several internal crates (called *tiers*) that build on each other.
//!
//! ## Architecture
//!
//! ```text
//! ┌─┐
//! │                   tier3                     │
//! │  ┌─┐  │
//! │  │              tier2                    │  │
//! │  │  ┌─┐  │  │
//! │  │  │            tier1                │  │  │
//! │  │  │  • Request/response types       │  │  │
//! │  │  │  • Pipeline orchestration       │  │  │
//! │  │  │  • Caching, dispatch, routing   │  │  │
//! │  │  │  • Compiles to native & WASM    │  │  │
//! │  │  └─┘  │  │
//! │  │  • FFmpeg decode (video, HEIC, EXR)   │  │
//! │  │  • Image codecs (JPEG-XL, WebP, SVG)  │  │
//! │  └─┘  │
//! │  • Sandbox + env checks                      │
//! │  • dlopen subprocess backends                │
//! │  • The `thumbrella` binary entrypoint        │
//! └─┘
//! ```
//!
//! | Crate | Role | Key modules |
//! |-------|------|-------------|
//! | [`tier1`] | Core types and pipeline - compiles to both native and `wasm32-unknown-unknown` | `request`, `result`, `spec`, `dispatch`, `pipeline`, `cache` |
//! | [`tier2`] | Media decoding - re-exports `tier1` and adds FFmpeg-backed renderers | `avdecode`, `renderer` |
//! | [`tier3`] | Full server - sandbox, env checks, pluggable backends, the `thumbrella` binary | `sandbox`, `env_check`, `renderer`, `scratch` |
//!
//! ### Supporting crates
//!
//! | Crate | Role |
//! |-------|------|
//! | [`ffs_build`] | Bundled static FFmpeg - version info, build metadata, vcpkg integration |
//! | [`ffs_check`] | Build-environment preflight checks for FFmpeg linkage |
//!
//! ## Naming conventions
//!
//! The project uses `tbr` as the short form of "thumbrella" in identifiers
//! (e.g. `TBR_VERSION`). Public API types follow a family naming scheme:
//!
//! | Prefix | Purpose |
//! |--------|---------|
//! | `Call*` | Outer HTTP envelope types (`CallRequest`, `CallResponse`, …) |
//! | `Thumb*` | Per-item types (`ThumbInput`, `ThumbResult`, `ThumbTrace`, …) |
//!
//! ## Where to start
//!
//! - **[`tier1::request`]** - incoming request structure
//! - **[`tier1::result`]** - thumbnail result types
//! - **[`tier1::dispatch`]** - the render dispatch table
//! - **[`tier1::pipeline`]** - end-to-end request pipeline
//! - **[`tier2::avdecode`]** - FFmpeg-based media decoding
//! - **[`tier3::sandbox`]** - server sandboxing and environment checks
//!
//! ## Build modes
//!
//! `tier1` has two build modes controlled by the `native` Cargo feature:
//!
//! | Mode | Feature flag | Target |
//! |------|-------------|--------|
//! | Native server | `native` (default) | `x86_64-*`, `aarch64-*` |
//! | Workers WASM | no `native` | `wasm32-unknown-unknown` |
//!
//! [`tier1`]: ../tier1/index.html
//! [`tier2`]: ../tier2/index.html
//! [`tier3`]: ../tier3/index.html
//! [`ffs_build`]: ../ffs_build/index.html
//! [`ffs_check`]: ../ffs_check/index.html
//! [`tier1::request`]: ../tier1/request/index.html
//! [`tier1::result`]: ../tier1/result/index.html
//! [`tier1::dispatch`]: ../tier1/dispatch/index.html
//! [`tier1::pipeline`]: ../tier1/pipeline/index.html
//! [`tier2::avdecode`]: ../tier2/avdecode/index.html
//! [`tier3::sandbox`]: ../tier3/sandbox/index.html
