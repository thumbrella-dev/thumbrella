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

//  ThumbRoute

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

//  Routing table

/// Route a (kind, extension) pair to the appropriate processing tier.
///
/// Uses [`format_manifest`] as the single source of truth: searches for the
/// lowest-tier entry matching `(kind, extension)`, then falls back to
/// kind-based defaults when the extension is not in the manifest.
///
/// Called during the inspect step once the file kind and canonical extension
/// are known.  Returns a best-effort recommendation — the cook may still
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
    /// Renderer name for trace attribution (e.g. `"3drender"`, `"libav"`).
    #[allow(dead_code)]
    pub renderer: &'static str,
    /// Whether tier 1 can extract an embedded thumbnail from this format
    /// without a full decode (shortcut pipeline).
    pub shortcut: bool,
}

/// Static manifest of every format Thumbrella knows about.
///
/// This is a fixed, hardcoded list — Tier 1 servers do not probe the
/// environment, so they must know the full universe of formats statically.
/// This is the single source of truth for both [`route`] and CLI diagnostics.
#[rustfmt::skip]
pub fn format_manifest() -> &'static [FormatEntry] {&[
    //  Tier 1 — pure Rust (image crate)
    FormatEntry {extension: "png", label: "PNG",
                kind: FileKind::Image,  tier: 1, renderer: "image_crate", shortcut: true },
    FormatEntry {extension: "gif", label: "GIF",
                kind: FileKind::Image,  tier: 1, renderer: "image_crate", shortcut: true },
    FormatEntry {extension: "webp", label: "Google",
                kind: FileKind::Image,  tier: 1, renderer: "image_crate", shortcut: true },
    FormatEntry {extension: "bmp", label: "Windows Bitmap",
                kind: FileKind::Image,  tier: 1, renderer: "image_crate", shortcut: true },
    FormatEntry {extension: "tiff", label: "Tagged interchange",
                kind: FileKind::Image,  tier: 1, renderer: "image_crate", shortcut: true },
    FormatEntry {extension: "ico", label: "Windows Icon",
                kind: FileKind::Image,  tier: 1, renderer: "image_crate", shortcut: true },
    //  Tier 2 — JPEG (baseline/progressive) via libav
    FormatEntry {extension: "jpeg", label: "JPEG",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: true },
    //  Tier 2 — libav / resvg / jxl-oxide
    FormatEntry {extension: "svg", label: "Scalable Vector",
                kind: FileKind::Vector, tier: 2, renderer: "resvg", shortcut: false},
    FormatEntry {extension: "jxl", label: "JPEG XL",
                kind: FileKind::Image,  tier: 2, renderer: "jxl_oxide", shortcut: false},
    FormatEntry {extension: "exr", label: "OpenEXR",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "hdr", label: "Radiance",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "avif", label: "AVIF",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "heic", label: "HEIC",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "dng", label: "DNG (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "cr2", label: "Canon (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "nef", label: "Nikon (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "arw", label: "Sony (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "orf", label: "Olympus (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "rw2", label: "Panasonic (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "pef", label: "Pentax (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "srw", label: "Samsung (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "raf", label: "Fuji (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "3fr", label: "Hasselblad (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "fff", label: "Hasselblad (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "iiq", label: "Phase One (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "mef", label: "Mamiya (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "rwl", label: "Leica (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "raw", label: "Generic (raw)",
                kind: FileKind::Image,  tier: 2, renderer: "raw_preview", shortcut: true },
    FormatEntry {extension: "psd", label: "Photoshop",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "pbm", label: "NetPBM",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "tga", label: "Targa",
                kind: FileKind::Image,  tier: 2, renderer: "libav", shortcut: false},
    //  Tier 2 — video (libav / ffmpeg)
    FormatEntry {extension: "mp4", label: "MPEG-4",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "mov", label: "QuickTime",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "avi", label: "Windows",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "webm", label: "Google",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "mkv", label: "Matroska",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "flv", label: "Flash Video",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "ts", label: "MPEG Transport Stream",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "3gp", label: "3GPP",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "ogv", label: "Ogg",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "wmv", label: "Windows Media",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "mpeg", label: "MPEG",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "m2ts", label: "Blu-ray Transport Stream",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "mxf", label: "Material Exchange Format",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "av1", label: "AV1 bitstream",
                kind: FileKind::Video,  tier: 2, renderer: "libav", shortcut: false},
    //  Tier 2 — audio (libav / ffmpeg)
    FormatEntry {extension: "mp3", label: "MPEG",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: true },
    FormatEntry {extension: "wav", label: "Windows",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "flac", label: "Free Lossless",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "ogg", label: "Vorbis",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "m4a", label: "MPEG-4",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "aac", label: "Apple",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "wma", label: "Windows Media",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "aiff", label: "Interchange",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    FormatEntry {extension: "opus", label: "Opus",
                kind: FileKind::Audio,  tier: 2, renderer: "libav", shortcut: false},
    //  Tier 2 — documents (thumbnail extraction from ZIP)
    FormatEntry {extension: "odt", label: "OpenOffice Document",
                kind: FileKind::Document,   tier: 2, renderer: "builtin", shortcut: true },
    FormatEntry {extension: "ods", label: "OpenOffice Spreadsheet",
                kind: FileKind::Document,   tier: 2, renderer: "builtin", shortcut: true },
    FormatEntry {extension: "odp", label: "OpenOffice Presentation",
                kind: FileKind::Document,   tier: 2, renderer: "builtin", shortcut: true },
    FormatEntry {extension: "docx", label: "Office Document",
                kind: FileKind::Document,   tier: 2, renderer: "builtin", shortcut: true },
    FormatEntry {extension: "xlsx", label: "Office Spreadsheet",
                kind: FileKind::Document,   tier: 2, renderer: "builtin", shortcut: true },
    FormatEntry {extension: "pptx", label: "Office presentation",
                kind: FileKind::Document,   tier: 2, renderer: "builtin", shortcut: true },
    //  Tier 3 — ffmpeg CLI: arithmetic JPEG + all image formats
    FormatEntry {extension: "jpeg", label: "JPEG (arithmetic)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "png", label: "PNG (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "webp", label: "WebP (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "bmp", label: "BMP (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "tiff", label: "TIFF (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "psd", label: "PSD (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "gif", label: "GIF (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "jp2", label: "JPEG 2000",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "pcx", label: "PCX",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "qoi", label: "QOI",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "xbm", label: "XBM",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "xpm", label: "XPM",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "xwd", label: "XWD",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "pam", label: "PAM",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "svg", label: "SVG (via ffmpeg)",
                kind: FileKind::Vector, tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "avif", label: "AVIF (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "heic", label: "HEIC (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    FormatEntry {extension: "heic", label: "HEIC (via ffmpeg)",
                kind: FileKind::Image,  tier: 3, renderer: "ffmpeg_cli", shortcut: false},
    //  Tier 3 — oiiotool: studio image formats
    FormatEntry {extension: "exr", label: "OpenEXR",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "hdr", label: "Radiance HDR",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "dpx", label: "DPX",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "cin", label: "Cineon",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "dds", label: "DirectDraw Surface",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "dcm", label: "DICOM",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "fits", label: "FITS",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "iff", label: "Interchange",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "pic", label: "Softimage",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "rla", label: "Wavefront image",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "sgi", label: "Silicon Graphics",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    FormatEntry {extension: "zfile", label: "RenderMan Depth",
                kind: FileKind::Image,  tier: 3, renderer: "oiiotool", shortcut: false},
    //  Tier 3 — subprocess: 3D geometry
    FormatEntry {extension: "glb", label: "Khronos Binary",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "gltf", label: "Khronos Asset",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "fbx", label: "Filmbox",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "dae", label: "Collada",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "dxf", label: "AutoCAD",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "off", label: "Object File Format",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "exo", label: "Exodus II",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "3ds", label: "Autodesk 3D Studio",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "gml", label: "CityGML",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "ply", label: "Stanford Polygon",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "pts", label: "Point Cloud",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "usdz", label: "OpenUSD Archive",
                kind: FileKind::Geometry,   tier: 3, renderer: "usdz", shortcut: false},
    FormatEntry {extension: "usd", label: "OpenUSD",
                kind: FileKind::Geometry,   tier: 3, renderer: "usdz", shortcut: false},
    FormatEntry {extension: "stl", label: "Stereolithography",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "obj", label: "Wavefront",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vrml", label: "Virtual Reality Markup",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vtk", label: "VTK Legacy",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vtu", label: "VTK XML UnstructuredGrid",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vtp", label: "VTK XML PolyData",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vti", label: "VTK XML ImageData",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vtr", label: "VTK XML RectGrid",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vts", label: "VTK XML StructGrid",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "vtm", label: "VTK XML MultiBlock",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "step", label: "CAD",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "iges", label: "Graphics Exchange",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
    FormatEntry {extension: "brep", label: "CASCADE",
                kind: FileKind::Geometry,   tier: 3, renderer: "f3d", shortcut: false},
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
/// assumed to handle everything — this is correct for the hosted-service
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
/// empty set means "tier 3 handles nothing" — every tier-3 format will
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
        None => true, // external tier3 server — assume full support
        Some(set) => set.contains(extension),
    }
}
