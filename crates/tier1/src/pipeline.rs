//! Pipeline step functions.
//!
//! Each step receives `&mut ThumbCook<S>` and mutates `cook.response` and
//! `cook.trace` in place.  Steps are called in sequence from
//! [`ThumbCook::cook`](crate::cook::ThumbCook::cook).
//!
//! # Processing steps
//!
//! Not all steps run for every request — the path taken depends on the file
//! type and what is available.
//!
//! - **preflight** — Validate inputs, check credentials and rate limits,
//!   reject obvious bad inputs early.
//!
//! - **connect** — Open the HTTP connection, read response headers, perform
//!   the ETag/validator freshness check, pull the first bytes, and sniff
//!   the file type.
//!
//! - **inspect** — Read format-specific metadata without processing pixels
//!   (dimensions, duration, page count, codec info, …).  Often happens
//!   during connect when a prefix read is enough.
//!
//! - **fallback** — Serve a pre-rendered placeholder for a file type that
//!   cannot be thumbnailed.  Tier 1 only; no pixel work required.
//!
//! - **shortcut** — Extract an embedded thumbnail already inside the file
//!   (EXIF JPEG, HEIC cover, DOCX/ODT preview image, …) and bypass render.
//!
//! - **render** — Decode pixels and produce a `RenderImage` buffer.
//!
//! - **handoff** — Forward the request to a higher tier when Tier 1 cannot
//!   handle it.  Backend abstracted behind `HandoffBackend`, mirroring
//!   how `HttpBuffer<S: HttpStream>` isolates the HTTP backend:
//!
//!   | Backend | When used |
//!   |---------|-----------|
//!   | `InProcessBackend` | single-binary / test builds |
//!   | `HttpBackend`      | native tier-1; plain HTTP POST to tier-2 |
//!   | `SubrequestBackend` | Cloudflare Worker; `workers_rs::Fetch` sub-request |
//!
//! - **deliver** — Crop/resize to the canonical config, flatten transparency,
//!   apply colour corrections, mozjpeg-encode, populate `cook.thumb`.
//!
//! # Build note
//!
//! This module must compile on `wasm32-unknown-unknown`.  Each step function
//! will be generic over `S: HttpStream`.

