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
        (FileKind::Image, Some("png" | "gif" | "webp" | "bmp" | "tiff"))
        | (FileKind::Image, None) =>
            ThumbRoute { tier: 1 },

        // ── Tier 2 — JPEG and specialty images via libav ──────────────────────
        // JPEG is listed in both tier 2 and tier 3.  Tier 2 handles standard
        // Huffman-coded JPEGs; tier 3's ffmpeg CLI fallback handles arithmetic-
        // coded JPEGs (SOF9) that libav's mjpeg decoder rejects.
        (FileKind::Image, Some("jpeg" | "jpg")) =>
            ThumbRoute { tier: 2 },

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
        // Specific extensions map to individual subprocess handlers.
        (FileKind::Geometry, _) =>
            ThumbRoute { tier: 3 },

        // ── Tier 3 — ffmpeg CLI: formats not in tier2's slim static build ────
        // JPEG2000 is supported by the system ffmpeg in the tier3 Docker image
        // (--enable-libopenjpeg) but not by tier2's minimal static build.
        (FileKind::Image, Some("jp2" | "j2k")) =>
            ThumbRoute { tier: 3 },

        // Catch-all: tier-1 placeholder.
        _ => ThumbRoute { tier: 1 },
    }
}

// ── Format manifest ───────────────────────────────────────────────────────────

/// A single format entry in the static dispatch manifest.
///
/// This is the authoritative list of every format Thumbrella can process
/// and the tier responsible for it.  Tier 1 servers use this to know what
/// formats exist (even if they can't render them).  The diag command uses
/// it to report tier-level format coverage.
#[derive(Debug, Clone)]
pub struct FormatEntry {
    /// Canonical file extension (e.g. `"glb"`, `"exr"`).
    pub extension: &'static str,
    /// Human-readable label (e.g. `"glTF Binary"`, `"OpenEXR"`).
    pub label: &'static str,
    /// FileKind category.
    pub kind: FileKind,
    /// Tier that processes this format.
    pub tier: u8,
    /// Renderer name for trace attribution (e.g. `"3drender"`, `"libav"`).
    #[allow(dead_code)]
    pub renderer: &'static str,
}

/// Static manifest of every format Thumbrella knows about.
///
/// This is a fixed, hardcoded list — Tier 1 servers do not probe the
/// environment, so they must know the full universe of formats statically.
/// This list is consumed by the `diag` command to report tier coverage.
pub fn format_manifest() -> &'static [FormatEntry] {
    &[
        // ── Tier 1 — pure Rust (image crate) ─────────────────────────────────
        FormatEntry { extension: "png",  label: "PNG",              kind: FileKind::Image, tier: 1, renderer: "image_crate" },
        FormatEntry { extension: "gif",  label: "GIF",              kind: FileKind::Image, tier: 1, renderer: "image_crate" },
        FormatEntry { extension: "webp", label: "WebP",             kind: FileKind::Image, tier: 1, renderer: "image_crate" },
        FormatEntry { extension: "bmp",  label: "BMP",              kind: FileKind::Image, tier: 1, renderer: "image_crate" },
        FormatEntry { extension: "tiff", label: "TIFF",             kind: FileKind::Image, tier: 1, renderer: "image_crate" },
        FormatEntry { extension: "ico",  label: "ICO",              kind: FileKind::Image, tier: 1, renderer: "image_crate" },

        // ── Tier 2 — JPEG (baseline/progressive) via libav ───────────────────
        FormatEntry { extension: "jpeg", label: "JPEG (standard)",  kind: FileKind::Image, tier: 2, renderer: "libav" },
        FormatEntry { extension: "jpg",  label: "JPEG (standard)",  kind: FileKind::Image, tier: 2, renderer: "libav" },

        // ── Tier 2 — libav / resvg / jxl-oxide ───────────────────────────────
        FormatEntry { extension: "svg",  label: "SVG",              kind: FileKind::Vector,  tier: 2, renderer: "resvg" },
        FormatEntry { extension: "jxl",  label: "JPEG XL",         kind: FileKind::Image,   tier: 2, renderer: "jxl_oxide" },
        FormatEntry { extension: "exr",  label: "OpenEXR",         kind: FileKind::Image,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "hdr",  label: "HDR / Radiance",  kind: FileKind::Image,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "avif", label: "AVIF",            kind: FileKind::Image,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "heic", label: "HEIC",            kind: FileKind::Image,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "heif", label: "HEIF",            kind: FileKind::Image,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "dng",  label: "DNG (raw)",       kind: FileKind::Image,   tier: 2, renderer: "raw_preview" },
        FormatEntry { extension: "cr2",  label: "CR2 (raw)",       kind: FileKind::Image,   tier: 2, renderer: "raw_preview" },
        FormatEntry { extension: "nef",  label: "NEF (raw)",       kind: FileKind::Image,   tier: 2, renderer: "raw_preview" },
        FormatEntry { extension: "psd",  label: "PSD",             kind: FileKind::Image,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "mp4",  label: "MP4 video",       kind: FileKind::Video,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "mov",  label: "QuickTime",       kind: FileKind::Video,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "avi",  label: "AVI",             kind: FileKind::Video,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "webm", label: "WebM",            kind: FileKind::Video,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "mkv",  label: "Matroska",        kind: FileKind::Video,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "mp3",  label: "MP3 audio",       kind: FileKind::Audio,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "wav",  label: "WAV audio",       kind: FileKind::Audio,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "flac", label: "FLAC audio",      kind: FileKind::Audio,   tier: 2, renderer: "libav" },
        FormatEntry { extension: "ogg",  label: "Ogg audio",       kind: FileKind::Audio,   tier: 2, renderer: "libav" },

        // ── Tier 3 — ffmpeg CLI: arithmetic JPEG + all image formats ──────────
        FormatEntry { extension: "jpeg", label: "JPEG (arithmetic)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "jpg",  label: "JPEG (arithmetic)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "png",  label: "PNG (via ffmpeg)",  kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "webp", label: "WebP (via ffmpeg)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "bmp",  label: "BMP (via ffmpeg)",  kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "tiff", label: "TIFF (via ffmpeg)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "psd",  label: "PSD (via ffmpeg)",  kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "gif",  label: "GIF (via ffmpeg)",  kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "jp2",  label: "JPEG 2000",         kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "j2k",  label: "JPEG 2000",         kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "pcx",  label: "PCX",               kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "qoi",  label: "QOI",               kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "xbm",  label: "XBM",               kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "xpm",  label: "XPM",               kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "xwd",  label: "XWD",               kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "pam",  label: "PAM",               kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "svg",  label: "SVG (via ffmpeg)",  kind: FileKind::Vector, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "avif", label: "AVIF (via ffmpeg)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "heic", label: "HEIC (via ffmpeg)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },
        FormatEntry { extension: "heif", label: "HEIF (via ffmpeg)", kind: FileKind::Image, tier: 3, renderer: "ffmpeg_cli" },

        // ── Tier 3 — oiiotool: studio image formats ──────────────────────────
        FormatEntry { extension: "exr",  label: "OpenEXR",           kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "sxr",  label: "OpenEXR",           kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "mxr",  label: "OpenEXR",           kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "hdr",  label: "Radiance HDR",      kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "rgbe", label: "Radiance HDR",      kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "dpx",  label: "DPX",               kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "cin",  label: "Cineon",            kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "dds",  label: "DirectDraw Surface",kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "fits", label: "FITS",              kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "iff",  label: "IFF",               kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "pic",  label: "Softimage PIC",     kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "rla",  label: "RLA",               kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "sgi",  label: "SGI",               kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "rgb",  label: "SGI RGB",           kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "rgba", label: "SGI RGBA",          kind: FileKind::Image, tier: 3, renderer: "oiiotool" },
        FormatEntry { extension: "zfile",label: "Renderman Z-file",  kind: FileKind::Image, tier: 3, renderer: "oiiotool" },

        // ── Tier 3 — subprocess: 3D geometry ──────────────────────────────────
        FormatEntry { extension: "glb",  label: "glTF Binary",     kind: FileKind::Geometry, tier: 3, renderer: "3drender" },
        FormatEntry { extension: "gltf", label: "glTF JSON",       kind: FileKind::Geometry, tier: 3, renderer: "3drender" },
        FormatEntry { extension: "usdz", label: "USDZ",            kind: FileKind::Geometry, tier: 3, renderer: "usdrender" },
        FormatEntry { extension: "usdc", label: "USDC",            kind: FileKind::Geometry, tier: 3, renderer: "usdrender" },
        FormatEntry { extension: "usda", label: "USDA",            kind: FileKind::Geometry, tier: 3, renderer: "usdrender" },
        FormatEntry { extension: "stl",  label: "STL",             kind: FileKind::Geometry, tier: 3, renderer: "stlrender" },
        FormatEntry { extension: "obj",  label: "Wavefront OBJ",   kind: FileKind::Geometry, tier: 3, renderer: "stlrender" },
    ]
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
