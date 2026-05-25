//! Dispatch routing — mapping (kind, extension) to a processing tier.
//!
//! # Tier definitions
//!
//! Each tier is a strict superset of the ones below it.  A Tier 3 server is a
//! complete standalone system; a Tier 1 server handles only its own slice and
//! falls back to placeholder icons for anything it cannot process.
//!
//! | Tier | Capabilities | Deployment target |
//! |------|-------------|-------------------|
//! | **1** | Pure-Rust decode (image crate).  Common raster formats, archives, plain text.  No rendering. | Cloudflare Workers (WASM) |
//! | **2** | Adds libav (ffmpeg) and a small number of system libraries.  Video keyframes, audio waveforms, HDR images, SVG rendering (resvg), complex documents. | Cloudflare Containers (lightweight Docker) |
//! | **3** | Adds subprocess-based renderers requiring heavy dependencies or a display server (xvfb).  3-D geometry, advanced simulation output, anything needing a full render pipeline. | High-memory Docker hosts (e.g. Replicate) |
//!
//! # Tier 1 CPU budget
//!
//! Cloudflare Workers enforces a **10 ms CPU time limit** per request.  Network
//! I/O (fetching the source URL) and subrequest time (dispatching to Tier 2)
//! do not count against this budget, but all in-process work does.  This means
//! Tier 1 on Workers is primarily a **routing and cache layer**:
//!
//! - Check cache.  Serve immediately on hit.
//! - Read first bytes, sniff file type, make routing decision.
//! - Dispatch as a subrequest to Tier 2 for anything that requires real decode work.
//!
//! Even formats the `image` crate supports (e.g. large JPEG, PNG) may exceed
//! the budget for non-trivial inputs.  Only truly trivial raster cases are
//! processed in-tier; everything else dispatches.
//!
//! SVG rendering via resvg is definitively Tier 2 — layout and rasterization
//! of even modest SVG files can consume tens of milliseconds of CPU.
//!
//! When a tier is not configured (no handoff target registered), the cook falls
//! back to a `Fallback` strategy — a pre-rendered placeholder icon is used and
//! the result is marked accordingly.  No error is surfaced to the client.
//!
//! # Tier 2 bypass
//!
//! Premium/authenticated customers can have the sniff-and-route step skipped
//! entirely via [`bypass`].  The request goes directly to Tier 2 without
//! fetching a single byte from the source URL at Tier 1.  Benefits:
//!
//! - Fewer IOPS on the customer's origin storage (no probe read).
//! - More predictable latency (no Tier 1 CPU time consumed on routing).
//!
//! This is a paid feature: it costs more to run (every request hits Tier 2)
//! but delivers a better experience for customers who value storage efficiency.
//!
//! # Cache invalidation note
//!
//! A per-route cache generation integer was considered but removed.  Cache
//! lookup must happen *before* sniffing — we need a cache hit to return before
//! spending any CPU on file type detection.  Since routing only happens after
//! sniffing, a route-level generation number cannot be part of the cache key
//! at the point where it would matter.  Any cache invalidation strategy will
//! need to be a global generation bump or a targeted URL-based purge.

use crate::media::FileKind;

// ── ThumbRoute ────────────────────────────────────────────────────────────────

/// Dispatch result for one item.
///
/// Produced by [`route`] or [`bypass`]; consumed by `ThumbCook` to decide
/// between local processing and a tier handoff.
///
/// `tier` is a **recommendation**, not a hard constraint.  The cook may
/// escalate to a higher tier at runtime — for example, if a TIFF turns out to
/// be 200 MB, or if the connect step reveals a codec that requires libav.
/// The `tier` recorded here is the *initial* routing decision; the actual tier
/// used is recorded in `ThumbTrace` after the cook completes.
///
/// If the required tier is not available (no handoff configured), the cook
/// falls back to a placeholder icon — the tier is still logged accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbRoute {
    /// Tier that should process this item (1 = this server, 2+ = handoff).
    pub tier: u8,
}

// ── Routing table ─────────────────────────────────────────────────────────────

/// Route a (kind, extension) pair to the appropriate processing tier.
///
/// Called during the inspect step once the file kind and canonical extension
/// are known.  Returns a best-effort recommendation — the cook may still
/// escalate to a higher tier at runtime based on file size, detected codec,
/// or other properties discovered during connect/inspect.
///
/// When the required tier is not available the cook degrades gracefully:
/// it uses the `Fallback` strategy and returns a placeholder icon.
pub fn route(kind: FileKind, extension: Option<&str>) -> ThumbRoute {
    match (kind, extension) {
        // ── Tier 1 — pure Rust (image crate) ─────────────────────────────────
        (FileKind::Image, Some("jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff"))
        | (FileKind::Image, None) =>
            ThumbRoute { tier: 1 },

        // ── Tier 1 — archives and text: placeholder icon, no pixel work ───────
        (FileKind::Archive | FileKind::Text | FileKind::Binary | FileKind::Unknown, _) =>
            ThumbRoute { tier: 1 },

        // ── Tier 2 — SVG/vector (resvg CPU cost exceeds Workers budget) ───────
        (FileKind::Vector, _) =>
            ThumbRoute { tier: 2 },

        // ── Tier 2 — libav: HDR/specialty images ──────────────────────────────
        (FileKind::Image, Some("exr" | "hdr" | "avif" | "heic" | "heif" | "jxl")) =>
            ThumbRoute { tier: 2 },

        // ── Tier 2 — camera raw containers (tier1 may still shortcut first) ─
        (FileKind::Image, Some(
            "dng" | "cr2" | "nef" | "arw" | "orf" | "rw2"
            | "pef" | "srw" | "raf" | "3fr" | "fff" | "iiq" | "raw"
        )) =>
            ThumbRoute { tier: 2 },

        // ── Tier 2 — libav: video keyframe extraction ─────────────────────────
        (FileKind::Video, _) =>
            ThumbRoute { tier: 2 },

        // ── Tier 2 — libav: audio waveform ───────────────────────────────────
        (FileKind::Audio, _) =>
            ThumbRoute { tier: 2 },

        // ── Tier 2 — documents (LibreOffice headless or similar) ──────────────
        (FileKind::Document, _) =>
            ThumbRoute { tier: 2 },

        // ── Tier 3 — subprocess renderers: 3-D geometry ───────────────────────
        // Requires a display server (xvfb) and heavy render dependencies.
        (FileKind::Geometry, _) =>
            ThumbRoute { tier: 3 },

        // Catch-all: tier-1 placeholder.
        _ => ThumbRoute { tier: 1 },
    }
}

/// Bypass sniffing and route directly to Tier 2.
///
/// Used for premium/authenticated customers where Tier 1 skips the probe read
/// entirely.  The request is forwarded to Tier 2 without fetching a single byte
/// from the source URL at Tier 1, reducing IOPS on the customer's origin storage.
///
/// The returned route always has `tier: 2`.  If no Tier 2 handoff is configured
/// the cook will fall back to a placeholder icon, same as any other unserviceable
/// tier.
pub fn bypass() -> ThumbRoute {
    ThumbRoute { tier: 2 }
}
