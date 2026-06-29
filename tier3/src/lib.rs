//! Tier 3 library.
//!
//! Tier 3 is the fully-featured thumbnail service.  It is a superset of tier 2:
//! all tier-2 rendering paths (FFmpeg, resvg, jxl-oxide, raw preview) are
//! available in-process.  On top of that, tier 3 adds a pluggable dispatch
//! table of optional renderer backends that are probed at startup:
//!
//! - **Shared library backends** — detected via `dlopen` (e.g. libpdfium).
//! - **Subprocess backends** — detected via `which` + a benign invocation
//!   (e.g. inkscape, blender, ffmpeg CLI).
//!
//! Only backends that are detected at startup are registered in the dispatch
//! table.  Missing backends are silently skipped at render time.
//!
//! # Environment check
//!
//! Tier 3 probes the host environment at startup and caches the results.
//! The `tier3 check` command prints a human-readable capability report.
//!
//! # Pipeline integration
//!
//! Tier 3 is registered as the in-process renderer on the tier 1 [`Runtime`].
//! It implements [`InProcessRenderer`] and receives each cook after the
//! inspect step.  The dispatch table is tried in order until a backend
//! claims the format.

pub use tier1::*;

pub mod env_check;
pub mod renderer;
pub mod sandbox;
pub mod scratch;
pub use renderer::Tier3Renderer;
