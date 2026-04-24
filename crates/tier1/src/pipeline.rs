//! Core processing pipeline.
//!
//! `process_item` is the single entry point for a fully-resolved `ItemRequest`.
//! It orchestrates the named processing steps below and emits `ItemEvent` values.
//!
//! # Processing steps
//!
//! Not all steps run for every request — the path taken depends on the file
//! type and what the caller asked for.  The named steps are:
//!
//! - **preflight** — Parse and validate the incoming request, check
//!   credentials and rate limits, reject obvious bad inputs early, and build
//!   the per-request context that all subsequent steps share.
//!
//! - **connect** — Open the HTTP connection to the media URL, read response
//!   headers, perform the ETag/validator freshness check, pull the first bytes,
//!   sniff the file type, and choose which branch to take next.
//!
//! - **inspect** — Read format-specific metadata without processing any pixels
//!   (dimensions, duration, page count, codec info, …).  Often runs as part of
//!   connect when a prefix read is enough; sometimes a separate pass.
//!
//! - **fallback** — Serve a pre-rendered placeholder thumbnail for a file type
//!   that cannot be thumbnailed.  Tier 1 only; no pixel work required.
//!
//! - **shortcut** — Extract an embedded thumbnail that already exists inside
//!   the file (EXIF JPEG, HEIC cover, DOCX/ODT preview image, …) and pass it
//!   to deliver without a full decode.  Tier 1 only.
//!
//! - **render** — Decode pixels and produce an image buffer.  Ranges from a
//!   simple raster load (Tier 1) through video keyframe seek (Tier 2) to
//!   offline rendering of 3-D models (Tier 3).
//!
//! - **handoff** — Forward the request to a higher tier when Tier 1 cannot
//!   handle it.  The higher tier streams its result back; Tier 1 proxies it.
//!   When implemented, the backend will be abstracted behind a `HandoffBackend`
//!   trait following the same pattern as [`crate::http_buf::HttpStream`]:
//!
//!   | Backend | When used |
//!   |---------|-----------|
//!   | `InProcessBackend` | single-binary / test builds; direct `async fn` call |
//!   | `HttpBackend`      | native tier-1 server; plain HTTP POST to tier-2 |
//!   | `SubrequestBackend` | Cloudflare Worker; `workers_rs::Fetch` sub-request |
//!
//!   `process_item<H: HandoffBackend>` will be generic over the backend so the
//!   pipeline itself stays target-agnostic.
//!
//! - **deliver** — Take the image buffer from shortcut, render, or a cached
//!   artifact and produce the final JPEG: crop/resize to the canonical config,
//!   flatten transparency, apply colour corrections, and mozjpeg-encode.
//!
//! # Build note
//!
//! This module must compile on `wasm32-unknown-unknown` without modification.
//! OS/thread-dependent code belongs in `native` feature-gated modules.

use crate::result::{ItemResponse, JobStatus};
use crate::request::ItemRequest;
use crate::source::canonical_url;

/// Process a single item and return the result.
///
/// This is the placeholder implementation.  Real decode and encode logic will
/// replace the stub body as the pipeline is built out.
///
/// The signature will grow to `process_item<H: HandoffBackend>(req: ItemRequest)`
/// once the handoff step is implemented, mirroring how `HttpBuffer<S: HttpStream>`
/// isolates the HTTP backend from the rest of the pipeline.
pub async fn process_item(req: ItemRequest) -> ItemResponse {
    let url = req.source.as_url().map(str::to_owned);
    let _canonical = req.source.as_url().and_then(canonical_url);

    // TODO: fetch → inspect → dispatch → render → encode
    ItemResponse {
        url: url.unwrap_or_default(),
        status: JobStatus::Failed,
        message: "pipeline not yet implemented".into(),
        ..ItemResponse::default()
    }
}
