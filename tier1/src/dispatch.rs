//! Dispatch routing - mapping (kind, extension) to a processing tier.
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
//! SVG rendering via resvg is definitively Tier 2 - layout and rasterization
//! of even modest SVG files can consume tens of milliseconds of CPU.
//!
//! When a tier is not configured (no handoff target registered), the cook falls
//! back to a `Fallback` strategy - a pre-rendered placeholder icon is used and
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
//! lookup must happen *before* sniffing - we need a cache hit to return before
//! spending any CPU on file type detection.  Since routing only happens after
//! sniffing, a route-level generation number cannot be part of the cache key
//! at the point where it would matter.  Any cache invalidation strategy will
//! need to be a global generation bump or a targeted URL-based purge.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::media::FileKind;

//  Builtin tier flags
//
// These live here (not in the native-only `check` module) because the
// always-compiled `cook` module reads them during the handoff chain.

/// Whether tier 2 is compiled into this binary (set at startup by tier2/tier3).
pub static TIER2_BUILTIN: AtomicBool = AtomicBool::new(false);

/// Whether tier 3 is compiled into this binary (set at startup by tier3).
pub static TIER3_BUILTIN: AtomicBool = AtomicBool::new(false);

/// Signal that tier 2 is built into this binary.  Call at startup from
/// `tier2` or `tier3` binaries.
pub fn mark_tier2_builtin() {
    TIER2_BUILTIN.store(true, Ordering::Release);
}

/// Signal that tier 3 is built into this binary.  Call at startup from
/// `tier3` binaries.
pub fn mark_tier3_builtin() {
    TIER3_BUILTIN.store(true, Ordering::Release);
}

/// True when at least one higher-tier renderer is compiled into this binary.
pub fn has_builtin_renderer() -> bool {
    TIER2_BUILTIN.load(Ordering::Acquire) || TIER3_BUILTIN.load(Ordering::Acquire)
}

//  ThumbRoute

/// Dispatch result for one item.
///
/// Produced by [`route`] or [`bypass`]; consumed by `ThumbCook` to decide
/// between local processing and a tier handoff.
///
/// `tier` is a **recommendation**, not a hard constraint.  The cook may
/// escalate to a higher tier at runtime - for example, if a TIFF turns out to
/// be 200 MB, or if the connect step reveals a codec that requires libav.
/// The `tier` recorded here is the *initial* routing decision; the actual tier
/// used is recorded in `ThumbTrace` after the cook completes.
///
/// If the required tier is not available (no handoff configured), the cook
/// falls back to a placeholder icon - the tier is still logged accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbRoute {
    /// Tier that should process this item (1 = this server, 2+ = handoff).
    pub tier: u8,
}

//  Routing table

/// Route a (kind, extension) pair to the appropriate processing tier.
///
/// Uses [`format_manifest`] as the single source of truth: searches for the
/// lowest-tier entry matching `(kind, extension)`, then falls back to
/// kind-based defaults when the extension is not in the manifest.
///
/// Called during the inspect step once the file kind and canonical extension
/// are known.  Returns a best-effort recommendation - the cook may still
/// escalate to a higher tier at runtime based on file size, detected codec,
/// or other properties discovered during connect/inspect.
///
/// When the required tier is not available the cook degrades gracefully:
/// it uses the `Fallback` strategy and returns a placeholder icon.
pub fn route(kind: FileKind, extension: Option<&str>) -> ThumbRoute {
    // Primary lookup: search the manifest for the lowest-tier match.
    // Some formats appear under multiple tiers (e.g. JPEG is tier 2 for
    // standard Huffman, tier 3 for arithmetic-coded fallback).  We always
    // route to the *lowest* tier that can handle the format.
    if let Some(ext) = extension {
        let manifest_tier = format_manifest()
            .iter()
            .filter(|e| e.kind == kind && e.extension == ext)
            .map(|e| e.tier)
            .min();
        if let Some(t) = manifest_tier {
            return ThumbRoute { tier: t };
        }
    }

    // Fallback: kind-based defaults for extensions not in the manifest.
    let tier = match kind {
        // Archives, text, binary, unknown: placeholder icon, no pixel work.
        FileKind::Archive | FileKind::Text | FileKind::Binary | FileKind::Unknown => 1,
        // Images: try image crate first (tier 1).
        FileKind::Image => 1,
        // Vector, video, audio, documents: require libav / resvg / headless.
        FileKind::Vector | FileKind::Video | FileKind::Audio | FileKind::Document => 2,
        // 3-D geometry: subprocess renderers with display server.
        FileKind::Geometry => 3,
    };
    ThumbRoute { tier }
}

//  Format manifest

/// Extension-only routing preview, before the file is sniffed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewRoute {
    /// Lowest tier the extension is registered for.
    pub tier: u8,
    /// Kind recorded for that manifest entry.
    pub kind: FileKind,
}

/// Preview the tier a URL extension is routed to, before the file is sniffed.
///
/// This is the preview used to decide whether a speculative handoff wakeup is
/// worth sending (see [`crate::prewarm`]).  It reads only [`format_manifest`] -
/// no `infer` sniffing, no HTTP - because it runs before `connect`/`inspect`.
///
/// Returns `None` when the extension is not in the manifest.  An unknown
/// extension is no evidence that any tier will be needed, so callers should not
/// start speculative work for it; formats reachable only by magic bytes (no
/// extension, or a generic one) simply miss out on the wakeup.
pub fn preview_route(extension: &str) -> Option<PreviewRoute> {
    format_manifest()
        .iter()
        .filter(|entry| entry.extension == extension)
        .min_by_key(|entry| entry.tier)
        .map(|entry| PreviewRoute { tier: entry.tier, kind: entry.kind })
}

//  Manifest queries
//
// Renderers ask these instead of keeping their own extension lists.  A format
// that gains a decoder is one manifest row, not one row plus a `matches!` in
// every tier that cares.

/// Every manifest row for `extension` - one per tier that can handle it.
pub fn entries_for(extension: &str) -> impl Iterator<Item = &'static FormatEntry> {
    format_manifest().iter().filter(move |entry| entry.extension == extension)
}

/// True when `extension` lists `handler` at `max_tier` or below.
pub fn handles(extension: &str, max_tier: u8, handler: Handler) -> bool {
    entries_for(extension).any(|entry| entry.tier <= max_tier && entry.handlers.contains(&handler))
}

/// Every extension whose row at *exactly* `tier` lists `handler`.
///
/// Tier-3 registers one handler per command and needs the full claim for that
/// command at that tier, so this is an exact-tier query rather than the `<=`
/// used by the render predicates.  Order follows the manifest.
pub fn extensions_for(handler: Handler, tier: u8) -> Vec<&'static str> {
    format_manifest()
        .iter()
        .filter(|entry| entry.tier == tier && entry.handlers.contains(&handler))
        .map(|entry| entry.extension)
        .collect()
}

/// True when `extension` is registered as an image format at `max_tier` or below.
pub fn handles_image(extension: &str, max_tier: u8) -> bool {
    entries_for(extension).any(|entry| entry.kind == FileKind::Image && entry.tier <= max_tier)
}

/// True when any tier records an embedded-thumbnail shortcut for `extension`.
///
/// The flag is per row, and an extension may have several rows (`webp` is
/// registered at tier 1 and tier 2), so this asks whether *any* row claims one.
pub fn shortcut_for(extension: &str) -> bool {
    entries_for(extension).any(|entry| entry.shortcut)
}

/// Extensions whose pixel data is scene-linear.
///
/// These need a gamma curve before delivery or they render dark and crushed.
/// Kept as a property rather than a manifest field because it is a property of
/// the pixel format, not of the tier that decodes it.
pub fn is_linear(extension: &str) -> bool {
    matches!(extension, "exr" | "hdr")
}

/// Extension aliases folded to their canonical form by
/// [`canonical_extension`](crate::pipeline::inspect::canonical_extension).
///
/// `(alias, canonical)`; every canonical value must be an extension the
/// manifest knows about.  A format decoded by name - rather than by magic
/// bytes - must know its own aliases, so this belongs next to the manifest
/// instead of in the sniffing code.
pub const EXTENSION_ALIASES: &[(&str, &str)] = &[
    // image formats
    ("jpg", "jpeg"),
    ("jpe", "jpeg"),
    ("tif", "tiff"),
    ("j2k", "jp2"),
    ("sxr", "exr"),
    ("mxr", "exr"),
    ("rgbe", "hdr"),
    ("rgb", "sgi"),
    ("rgba", "sgi"),
    ("int", "sgi"),
    ("inta", "sgi"),
    ("bw", "sgi"),
    ("iris", "sgi"),
    ("bm", "xbm"),
    ("pgm", "pbm"),
    ("ppm", "pbm"),
    ("pnm", "pbm"),
    ("pfm", "pbm"),
    ("heics", "heic"),
    ("heif", "heic"),
    // video / audio
    ("mpg", "mpeg"),
    ("mpe", "mpeg"),
    ("mts", "m2ts"),
    ("m4v", "mp4"),
    ("m4b", "m4a"),
    ("mid", "midi"),
    ("aifc", "aiff"),
    // documents
    ("doc", "docx"),
    ("xls", "xlsx"),
    ("ppt", "pptx"),
    // vector
    ("svgz", "svg"),
    // 3D geometry
    ("wrl", "vrml"),
    ("stp", "step"),
    ("stpnc", "step"),
    ("p21", "step"),
    ("210", "step"),
    ("igs", "iges"),
    ("ex2", "exo"),
    ("e", "exo"),
    // camera raw
    ("crw", "cr2"),
    // web
    ("htm", "html"),
    ("mht", "mhtml"),
];

//  Handler

/// Renderer that processes a format at the tier recorded in the manifest.
///
/// Every renderer-specific extension list elsewhere in the workspace
/// (`tier2::renderer::is_raw_format`, `tier3::renderer::is_oiio_format`, …)
/// is a hand-copied projection of this table.  They now ask the manifest
/// instead of repeating the extension names; this enum is what makes the
/// question answerable without string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handler {
    /// The `image` crate (tier 1 build, and tier 2 for PNG/TGA).
    ImageCrate,
    /// libav / ffmpeg libraries linked into tier 2.
    Libav,
    /// `jxl-oxide`, pure-Rust JPEG XL decoder.
    JxlOxide,
    /// `resvg`, pure-Rust SVG rasteriser.
    Resvg,
    /// Embedded JPEG preview extracted from a TIFF-container camera raw.
    RawPreview,
    /// `image` crate extras (icons, PCX, QOI-era formats the main build lacks).
    ImageExtras,
    /// Sparse DDS decoder.
    Dds,
    /// OpenEXR decoder.
    Exr,
    /// `ffmpeg` binary.
    FfmpegCli,
    /// `oiiotool` binary.
    Oiio,
    /// `soffice --headless`.
    Soffice,
    /// `pdftoppm` binary.
    Pdftoppm,
    /// `playwright-cli` headless browser.
    Playwright,
    /// Blender Workbench script.
    Blender,
    /// F3D geometry renderer.
    F3d,
    /// `usd-core` Python extractor used by the USDZ handler before F3D.
    UsdCore,
}

impl Handler {
    /// Name used in diagnostics and traces (e.g. `"libav"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Handler::ImageCrate => "image_crate",
            Handler::Libav => "libav",
            Handler::JxlOxide => "jxl_oxide",
            Handler::Resvg => "resvg",
            Handler::RawPreview => "raw_preview",
            Handler::ImageExtras => "image_extras",
            Handler::Dds => "dds",
            Handler::Exr => "exr",
            Handler::FfmpegCli => "ffmpeg_cli",
            Handler::Oiio => "oiiotool",
            Handler::Soffice => "soffice",
            Handler::Pdftoppm => "pdftoppm",
            Handler::Playwright => "playwright_cli",
            Handler::Blender => "blender",
            Handler::F3d => "f3d",
            Handler::UsdCore => "usd-core",
        }
    }
}

use Handler::*;

//  FormatEntry

/// A single format entry in the static dispatch manifest.
///
/// This is the authoritative list of every format Thumbrella can process
/// and the tier responsible for it.  Tier 1 servers use this to know what
/// formats exist (even if they can't render them).  The `check` and `formats`
/// CLI commands use it to report tier-level format coverage.
///
/// This table is the single source of truth for both runtime routing
/// ([`route`]) and diagnostic reporting.
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
    /// Renderers that can process this format at this tier, preferred first.
    ///
    /// More than one entry means either a fallback chain (Blender then F3D) or
    /// a component step (the USDZ handler extracts with `usd-core` before
    /// rendering with F3D).
    pub handlers: &'static [Handler],
    /// Whether tier 1 can extract an embedded thumbnail from this format
    /// without a full decode (shortcut pipeline).
    pub shortcut: bool,
}

impl FormatEntry {
    /// Human-readable handler chain (e.g. `"blender/f3d"`).
    pub fn handlers_label(&self) -> String {
        self.handlers.iter().map(|h| h.as_str()).collect::<Vec<_>>().join("/")
    }
}

/// Static manifest of every format Thumbrella knows about.
///
/// This is a fixed, hardcoded list - Tier 1 servers do not probe the
/// environment, so they must know the full universe of formats statically.
/// This is the single source of truth for both [`route`] and CLI diagnostics.
#[rustfmt::skip]
pub fn format_manifest() -> &'static [FormatEntry] {&[
    //  Tier 1 - pure Rust (image crate)
    FormatEntry {extension: "png", label: "Portable Network Graphics",
                kind: FileKind::Image,  tier: 1, handlers: &[ImageCrate], shortcut: true },
    FormatEntry {extension: "gif", label: "Graphics Interchange Format",
                kind: FileKind::Image,  tier: 1, handlers: &[ImageCrate], shortcut: true },
    FormatEntry {extension: "webp", label: "Google VP8 Image",
                kind: FileKind::Image,  tier: 1, handlers: &[ImageCrate], shortcut: true },
    FormatEntry {extension: "bmp", label: "Windows Bitmap",
                kind: FileKind::Image,  tier: 1, handlers: &[ImageCrate], shortcut: true },
    FormatEntry {extension: "tiff", label: "Tagged interchange",
                kind: FileKind::Image,  tier: 1, handlers: &[ImageCrate], shortcut: true },
    FormatEntry {extension: "ico", label: "Windows Icon",
                kind: FileKind::Image,  tier: 1, handlers: &[ImageCrate], shortcut: true },
    //  Tier 2 - JPEG (baseline/progressive) via libav
    FormatEntry {extension: "jpeg", label: "JPEG",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: true },
    //  Tier 2 - libav / resvg / jxl-oxide
    FormatEntry {extension: "webp", label: "Google VP8 Image",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageCrate], shortcut: false},    
    FormatEntry {extension: "svg", label: "Scalable Vector",
                kind: FileKind::Vector, tier: 2, handlers: &[Resvg], shortcut: false},
    FormatEntry {extension: "jxl", label: "JPEG XL",
                kind: FileKind::Image,  tier: 2, handlers: &[JxlOxide], shortcut: false},
    FormatEntry {extension: "exr", label: "OpenEXR",
                kind: FileKind::Image,  tier: 2, handlers: &[Exr], shortcut: false},
    FormatEntry {extension: "dds", label: "DirectDraw Surface",
                kind: FileKind::Image,  tier: 2, handlers: &[Dds], shortcut: false},
    FormatEntry {extension: "icns", label: "Apple Icon",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "ora", label: "OpenRaster",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "otb", label: "OTA Bitmap",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "pcx", label: "PCX",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "sgi", label: "Silicon Graphics",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "wbmp", label: "Wireless Bitmap",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "xbm", label: "XBM",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "xpm", label: "XPM",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageExtras], shortcut: false},
    FormatEntry {extension: "hdr", label: "Radiance",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "heic", label: "HEIC",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "avif", label: "AVIF",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "jp2", label: "JPEG 2000",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "dng", label: "DNG (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "cr2", label: "Canon (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "nef", label: "Nikon (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "arw", label: "Sony (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "orf", label: "Olympus (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "rw2", label: "Panasonic (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "pef", label: "Pentax (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "srw", label: "Samsung (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "raf", label: "Fuji (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "3fr", label: "Hasselblad (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "fff", label: "Hasselblad (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "iiq", label: "Phase One (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "mef", label: "Mamiya (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "rwl", label: "Leica (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "raw", label: "Generic (raw)",
                kind: FileKind::Image,  tier: 2, handlers: &[RawPreview], shortcut: true },
    FormatEntry {extension: "psd", label: "Photoshop",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "pbm", label: "NetPBM",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "pam", label: "Portable Arbitrary Map",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "qoi", label: "Quite OK Image",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "xwd", label: "X Window Dump",
                kind: FileKind::Image,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "tga", label: "Targa",
                kind: FileKind::Image,  tier: 2, handlers: &[ImageCrate], shortcut: false},
    //  Tier 2 - video (libav / ffmpeg)
    FormatEntry {extension: "mp4", label: "MPEG-4",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "mov", label: "QuickTime",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "avi", label: "Windows",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "webm", label: "Google",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "mkv", label: "Matroska",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "ts", label: "MPEG Transport Stream",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "3gp", label: "3GPP",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "ogv", label: "Ogg",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "wmv", label: "Windows Media",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "mpeg", label: "MPEG",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "m2ts", label: "Blu-ray Transport Stream",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "mxf", label: "Material Exchange Format",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "av1", label: "AV1 bitstream",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "flv", label: "Flash Video",
                kind: FileKind::Video,  tier: 2, handlers: &[Libav], shortcut: false},
    //  Tier 2 - audio (libav / ffmpeg)
    FormatEntry {extension: "mp3", label: "MPEG",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: true },
    FormatEntry {extension: "wav", label: "Windows",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "flac", label: "Free Lossless",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "ogg", label: "Vorbis",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "m4a", label: "MPEG-4",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "aac", label: "Apple",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "wma", label: "Windows Media",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "aiff", label: "Interchange",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    FormatEntry {extension: "opus", label: "Opus",
                kind: FileKind::Audio,  tier: 2, handlers: &[Libav], shortcut: false},
    //  Tier 3 - office documents.  The ZIP shortcut extracts embedded raster
    //  thumbnails first; anything else (no thumbnail, EMF/WMF-only preview,
    //  legacy binary formats) is rendered by `soffice --headless`.
    FormatEntry {extension: "odt", label: "OpenOffice Document",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "ods", label: "OpenOffice Spreadsheet",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "odp", label: "OpenOffice Presentation",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "odg", label: "OpenOffice Drawing",
                kind: FileKind::Vector,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "docx", label: "Office Document",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "xlsx", label: "Office Spreadsheet",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "pptx", label: "Office presentation",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "rtf", label: "Rich Text Format",
                kind: FileKind::Document,   tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "pages", label: "Apple Pages",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "numbers", label: "Apple Numbers",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "key", label: "Apple Keynote",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "xlsm", label: "Excel Macro-Enabled Spreadsheet",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "xlsb", label: "Excel Binary Spreadsheet",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "xltx", label: "Excel Spreadsheet Template",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "xltm", label: "Excel Macro-Enabled Template",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: true },
    FormatEntry {extension: "xlt", label: "Excel Legacy Template",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "xlm", label: "Excel Macro Sheet",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "xlw", label: "Excel Workspace",
                kind: FileKind::Document, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "emf", label: "Enhanced Metafile",
                kind: FileKind::Vector, tier: 3, handlers: &[Soffice], shortcut: false },
    FormatEntry {extension: "wmf", label: "Windows Metafile",
                kind: FileKind::Vector, tier: 3, handlers: &[Soffice], shortcut: false },
    //  Tier 3 - documents (PDF first-page render via pdftoppm subprocess)
    FormatEntry {extension: "pdf", label: "PDF Document",
                kind: FileKind::Document,   tier: 3, handlers: &[Pdftoppm], shortcut: false },
    //  Tier 3 - web pages (headless browser snapshot via playwright-cli)
    FormatEntry {extension: "html", label: "HTML Web Page",
                kind: FileKind::Document,   tier: 3, handlers: &[Playwright], shortcut: false },
    FormatEntry {extension: "xhtml", label: "XHTML Web Page",
                kind: FileKind::Document,   tier: 3, handlers: &[Playwright], shortcut: false },
    FormatEntry {extension: "mhtml", label: "MHTML Web Archive",
                kind: FileKind::Document,   tier: 3, handlers: &[Playwright], shortcut: false },
    //  Tier 3 - ffmpeg CLI as a last resort.  Every format below is decoded by
    //  the builtin libav or image crates at tier 2; these rows only matter when
    //  a deployment runs the external tool instead (or a variant the builtin
    //  build cannot handle turns up).
    FormatEntry {extension: "flv", label: "Flash Video",
                kind: FileKind::Video,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    //  Arithmetic-coded JPEG has no decoder in tier 2 - libav's mjpeg decoder
    //  and the Rust jpeg decoders both reject SOF9/SOF10.
    FormatEntry {extension: "jpeg", label: "JPEG (arithmetic)",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "psd", label: "PSD (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "pcx", label: "PCX",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "qoi", label: "QOI",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "xbm", label: "XBM",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "xpm", label: "XPM",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "xwd", label: "XWD",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "pam", label: "PAM",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    // Tier-3 fallbacks for formats tier 2 decodes with libav.  Only reached when
    // tier 2 is unavailable or libav lacks the codec, but the tier-3 handler
    // registry claims them, so the manifest has to say so too.
    FormatEntry {extension: "avif", label: "AVIF (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "jp2", label: "JPEG 2000 (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    FormatEntry {extension: "dcm", label: "DICOM",
                kind: FileKind::Image,  tier: 3, handlers: &[FfmpegCli], shortcut: false},
    //  Tier 3 - oiiotool: studio image formats
    FormatEntry {extension: "hdr", label: "Radiance HDR",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "dpx", label: "DPX",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "cin", label: "Cineon",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "fits", label: "FITS",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "iff", label: "Interchange",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "pic", label: "Softimage",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "rla", label: "Wavefront image",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    FormatEntry {extension: "zfile", label: "RenderMan Depth",
                kind: FileKind::Image,  tier: 3, handlers: &[Oiio], shortcut: false},
    //  Tier 3 - subprocess: 3D geometry
    FormatEntry {extension: "abc", label: "Alembic",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender], shortcut: false},
    FormatEntry {extension: "blend", label: "Blender",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender], shortcut: true},
    FormatEntry {extension: "glb", label: "Khronos Binary",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "gltf", label: "Khronos Asset",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "fbx", label: "Filmbox",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "dae", label: "Collada",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "dxf", label: "AutoCAD",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "off", label: "Object File Format",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "exo", label: "Exodus II",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "3ds", label: "Autodesk 3D Studio",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "gml", label: "CityGML",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "ply", label: "Stanford Polygon",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "pts", label: "Point Cloud",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "usdz", label: "OpenUSD Archive",
                kind: FileKind::Geometry,   tier: 3, shortcut: false,
                handlers: &[Blender, UsdCore, F3d] },
    FormatEntry {extension: "usd", label: "OpenUSD",
                kind: FileKind::Geometry,   tier: 3, shortcut: false,
                handlers: &[Blender, UsdCore, F3d] },
    FormatEntry {extension: "usda", label: "OpenUSD ASCII",
                kind: FileKind::Geometry,   tier: 3, shortcut: false,
                handlers: &[Blender, UsdCore, F3d] },
    FormatEntry {extension: "usdc", label: "OpenUSD Crate",
                kind: FileKind::Geometry,   tier: 3, shortcut: false,
                handlers: &[Blender, UsdCore, F3d] },
    FormatEntry {extension: "stl", label: "Stereolithography",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "obj", label: "Wavefront",
                kind: FileKind::Geometry,   tier: 3, handlers: &[Blender, F3d], shortcut: false},
    FormatEntry {extension: "vrml", label: "Virtual Reality Markup",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vtk", label: "VTK Legacy",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vtu", label: "VTK XML UnstructuredGrid",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vtp", label: "VTK XML PolyData",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vti", label: "VTK XML ImageData",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vtr", label: "VTK XML RectGrid",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vts", label: "VTK XML StructGrid",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "vtm", label: "VTK XML MultiBlock",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "step", label: "CAD",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "iges", label: "Graphics Exchange",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
    FormatEntry {extension: "brep", label: "CASCADE",
                kind: FileKind::Geometry,   tier: 3, handlers: &[F3d], shortcut: false},
]}

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

//  Tier 3 format-availability registry

/// Which extensions tier 3 can handle *in the current process*.
///
/// Populated at startup by tier 3 based on the environment probe
/// ([`probe_environment`]).  When `None` (the default), tier 3 is
/// assumed to handle everything - this is correct for the hosted-service
/// case where tier 3 runs as a separate, fully-provisioned server.
///
/// When `Some`, only the listed extensions are considered available.
/// Extensions not in the set will cause the fallback chain to skip tier 3
/// and go directly to a placeholder.
static TIER3_AVAILABLE_EXTS: std::sync::RwLock<Option<std::collections::HashSet<String>>> =
    std::sync::RwLock::new(None);

/// Register the set of extensions tier 3 can handle in this process.
///
/// Call once at startup, before any requests are served.  Passing an
/// empty set means "tier 3 handles nothing" - every tier-3 format will
/// fall through to a placeholder.
pub fn set_tier3_available_extensions(exts: std::collections::HashSet<String>) {
    *TIER3_AVAILABLE_EXTS.write().unwrap() = Some(exts);
}

/// Return `true` if tier 3 can handle `extension`.
///
/// When the registry has never been populated (external tier 3 server),
/// returns `true` unconditionally.
pub fn tier3_can_handle(extension: &str) -> bool {
    match TIER3_AVAILABLE_EXTS.read().unwrap().as_ref() {
        None => true, // external tier3 server - assume full support
        Some(set) => set.contains(extension),
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    #[test]
    fn renderers_derive_capability_from_the_manifest() {
        // Raw-preview extraction follows the manifest, including the raw
        // formats tier 2's old hand-written list had missed.
        for ext in ["dng", "cr2", "raf", "fff", "iiq", "mef", "rwl", "raw"] {
            assert!(handles(ext, 2, Handler::RawPreview), "{ext} should use raw_preview");
        }
        assert!(!handles("exr", 2, Handler::RawPreview));

        // Image lookups include libav-only formats, exclude tier-3-only ones.
        assert!(handles_image("pbm", 2));
        assert!(handles_image("heic", 2));
        assert!(!handles_image("dpx", 2));

        assert!(is_linear("exr") && is_linear("hdr") && !is_linear("png"));

        // Two rows: libav at tier 2, ffmpeg_cli at tier 3.
        assert_eq!(entries_for("jpeg").count(), 2);

        // Handler labels keep the diagnostic strings the `formats` CLI prints.
        assert_eq!(entries_for("usdz").next().unwrap().handlers_label(), "blender/usd-core/f3d");
        assert_eq!(entries_for("glb").next().unwrap().handlers_label(), "blender/f3d");
        assert_eq!(entries_for("dng").next().unwrap().handlers_label(), "raw_preview");
    }

    #[test]
    fn tier3_handler_lists_come_from_the_manifest() {
        // These are what the tier-3 handler registry declares, so the probes
        // and the render path cannot disagree about who owns a format.
        assert_eq!(extensions_for(Handler::Playwright, 3), ["html", "xhtml", "mhtml"]);
        assert_eq!(extensions_for(Handler::Pdftoppm, 3), ["pdf"]);
        assert_eq!(extensions_for(Handler::Oiio, 3).len(), 8);
        assert_eq!(extensions_for(Handler::Soffice, 3).len(), 20);

        // DICOM is decoded by the ffmpeg CLI, not oiiotool.
        assert!(extensions_for(Handler::FfmpegCli, 3).contains(&"dcm"));
        assert!(!extensions_for(Handler::Oiio, 3).contains(&"dcm"));

        // Tier-3 fallbacks the registry claimed for formats tier 2 decodes.
        let ffmpeg = extensions_for(Handler::FfmpegCli, 3);
        for ext in ["flv", "avif", "jp2", "psd", "pcx", "xbm", "xpm", "qoi", "xwd", "pam"] {
            assert!(ffmpeg.contains(&ext), "{ext} missing from ffmpeg_cli");
        }
    }
}
