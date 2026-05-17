//! Pipeline step: **shortcut** — extract an embedded thumbnail without full decode.
//!
//! Uses a two-phase read strategy to minimise network I/O:
//!
//! 1. **Header scan** — reads a small prefix (typically served from the
//!    `inspect` page cache with zero additional network I/O) to locate the
//!    embedded thumbnail's byte range within the remote file.
//!
//! 2. **Targeted fetch** — calls `read_at(thumb_offset, thumb_len)` to pull
//!    exactly the thumbnail bytes, then closes the connection immediately.
//!
//! When no embedded thumbnail is found, the HTTP connection is left open and
//! [`super::deliver`] runs next.
//!
//! Supported paths:
//! - JPEG: EXIF IFD1 `JPEGInterchangeFormat` thumbnail
//! - TIFF: embedded JPEG via IFD traversal
//! - ZIP containers (ODT, DOCX, …): single tail Range fetch that covers both
//!   the Central Directory and the embedded thumbnail data, with no further
//!   requests needed

use std::collections::{HashSet, VecDeque};
use web_time::Instant;

use image::{DynamicImage, imageops::FilterType};

use crate::cook::ThumbCook;
use crate::http_buf::HttpStream;
use crate::media::FileKind;
use crate::result::RenderHandler;
use crate::spec::ThumbnailConfig;

/// Header bytes needed to locate the embedded thumbnail span.
///
/// Matched to `inspect::SNIFF_LEN` so this read is served from the page
/// cache built during `inspect` with zero additional network I/O.
/// EXIF IFD structures for all common camera formats fit within 2 KiB.
const HEADER_SCAN: usize = 4 * 1024;

/// Tail bytes to fetch for the ZIP container shortcut.
///
/// Tail bytes to fetch for the ZIP container shortcut.
///
/// Sizing rationale (ODT test file):
/// - Central Directory: 649 bytes from EOF
/// - Thumbnail PNG (83.7 KB stored): starts 88.3 KB from EOF
/// - 128 KB tail gives ~40 KB margin over the observed minimum, comfortably
///   covering LibreOffice thumbnails up to roughly 120 KB (256×256 px, complex
///   content).  DOCX thumbnails (JPEG) are much smaller — typically under 50 KB.
///
/// Tier 1 default is 128 KiB; tier 2 uses 2 MiB to cover larger documents.
/// The actual value at runtime comes from `cook.runtime.shortcut_limits.zip_tail_size`.
// ZIP_TAIL_SIZE is now runtime-configurable via
// cook.runtime.shortcut_limits.zip_tail_size.
// See spec::ShortcutLimits for tier-specific values.

/// Header bytes for the camera-raw shortcut.
///
/// TIFF-based raw formats (DNG, CR2, NEF, …) embed a full JPEG preview inside
/// a SubIFD.  The SubIFD record offsets are referenced from IFD0, but the
/// SubIFD DATA itself can lie well past the 4 KiB standard `HEADER_SCAN`
/// window.  32 KiB covers the IFD metadata of all common camera models; the
/// preview JPEG is fetched separately via a targeted Range request (same as
/// the EXIF path).
const RAW_HEADER_SCAN: usize = 32 * 1024;

/// Known thumbnail paths inside ZIP-based container formats, checked in order.
const ZIP_THUMB_NAMES: &[&str] = &[
    "Thumbnails/thumbnail.png",   // ODT / ODS / ODP (ODF family)
    "docProps/thumbnail.jpeg",    // DOCX / XLSX / PPTX (OOXML family)
    "docProps/thumbnail.jpg",     // OOXML variant
    "docProps/thumbnail.png",     // OOXML variant
];

// ── Progressive JPEG shortcut ─────────────────────────────────────────────────

// MAX_PROGRESSIVE_PIXELS is now runtime-configurable via
// cook.runtime.shortcut_limits.max_progressive_pixels.
// See spec::ShortcutLimits for tier-specific values.

/// How many bytes of a progressive JPEG to fetch before attempting a decode.
///
/// Progressive JPEGs interleave frequency bands across the whole image rather
/// than storing one tile at a time.  The first scan contains all DC (lowest-
/// frequency) coefficients: `(w * h) / 42` bytes is empirically sufficient to
/// reconstruct a coarse whole-frame image for most camera/web JPEGs.
/// 10 KiB is the minimum to ensure we have a complete SOF+SOS header.
fn progressive_partial_read_bytes(w: Option<u32>, h: Option<u32>) -> usize {
    let estimated = w.zip(h)
        .map(|(w, h)| ((w as u64 * h as u64) / 42) as usize)
        .unwrap_or(256 * 1024);
    estimated.max(10 * 1024)
}

/// Extract JPEG image dimensions from a raw byte prefix without a full decode.
fn jpeg_source_dimensions(data: &[u8]) -> (Option<u32>, Option<u32>) {
    let mut props = serde_json::json!({});
    super::inspect::inspect_image_properties(data, &mut props);
    let obj = props.as_object().unwrap();
    let w = obj.get("width").and_then(|v| v.as_u64()).map(|n| n as u32);
    let h = obj.get("height").and_then(|v| v.as_u64()).map(|n| n as u32);
    (w, h)
}

/// Read a budget-limited prefix of a progressive JPEG and decode it using
/// `image::load_from_memory` (zune-jpeg backend).
///
/// Progressive JPEG serialises all DC coefficients (the lowest-frequency
/// component) before any AC refinement.  Fetching `(w * h) / 42` bytes is
/// enough to reconstruct a full-frame preview for most images.  We append an
/// EOI marker unconditionally so the decoder treats the truncated stream as a
/// complete file — this is cheaper than a failed first attempt.
///
/// The shortcut works for files of any size; there is no content-length gate.
///
/// The `HttpBuffer` is rewound to byte 0, capped at the byte budget via an
/// artificial EOF, and switched to streaming mode so new bytes bypass the page
/// cache (the already-cached header pages remain available).
async fn try_progressive_jpeg_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let config = &ThumbnailConfig::CANONICAL;

    // Read a heuristic prefix: (w * h) / 42 bytes covers the first progressive
    // scan for typical camera images.  The already-fetched inspect pages are
    // served from the page cache; only bytes beyond that come from the network.
    // The divisor 42 is empirically derived — see progressive_partial_read_bytes
    // in the thumbrella-tier1 prototype.
    //
    // We need the source dimensions to size the window, so peek at the header
    // bytes already in the page cache (zero network cost).
    let (src_w, src_h, fetch_target) = {
        let header = match cook.http_read_at(0, HEADER_SCAN).await {
            Ok(b) => b,
            Err(_) => return,
        };
        let (w, h) = jpeg_source_dimensions(&header);
        let pixel_count = w.unwrap_or(0) as u64 * h.unwrap_or(0) as u64;
        // pixel_count == 0 means we couldn't read the SOF marker (e.g. large
        // EXIF/ICC blocks push it past HEADER_SCAN).  Don't attempt a partial
        // decode of an image whose size we cannot verify.
        if pixel_count == 0 || pixel_count > cook.runtime.shortcut_limits.max_progressive_pixels {
            return;
        }
        let target = progressive_partial_read_bytes(w, h);
        (w.unwrap_or(0), h.unwrap_or(0), target)
    };

    cook.http_rewind();
    cook.http_set_eof(fetch_target as u64);
    cook.http_enter_streaming_mode();

    let mut data = Vec::with_capacity(fetch_target);
    {
        let mut tmp = vec![0u8; 32 * 1024];
        loop {
            match cook.http_read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => data.extend_from_slice(&tmp[..n]),
                Err(_) => return,
            }
        }
    }

    cook.http_clear_eof();

    if data.len() < 16 {
        return;
    }

    // Append EOI — harmless if already present, required for truncated streams.
    data.extend_from_slice(&[0xFF, 0xD9]);

    let t_render = Instant::now();
    let img = match image::load_from_memory(&data) {
        Ok(i) => i,
        Err(_) => return,
    };
    let decode_secs = t_render.elapsed().as_secs_f64();
    let color_type  = img.color();
    let img         = pre_scale_to_target(img, config.exact_width, config.exact_height);
    let dl_bytes    = cook.http_bytes_fetched();

    cook.render_renderer    = Some("shortcut/progressive".into());
    cook.render_handler     = RenderHandler::Builtin;
    cook.tel_decode_secs    = decode_secs;
    cook.out_download_bytes = dl_bytes;
    cook.render_is_progressive_partial = true;  // Mark as partial decode to suppress pixel-art heuristic
    if src_w > 0 && src_h > 0 {
        cook.media.properties = Some(image_properties(src_w, src_h, color_type));
    }

    cook.http_close().await;
    cook.render_image = Some(img);
}

// ── EXIF embedded thumbnail shortcut ────────────────────────────────────────

/// Attempt to serve a thumbnail from an embedded JPEG preview.
///
/// Covers two source formats:
/// - **JPEG**: EXIF APP1 IFD1 `JPEGInterchangeFormat` thumbnail.
/// - **TIFF**: embedded JPEG found via IFD chain traversal.
///
/// Reads the header from the page cache (zero extra network I/O), issues one
/// targeted Range request for the embedded thumbnail bytes, then closes the
/// connection.  Returns without touching `cook.render_image` when no embedded thumbnail
/// is found or decoding fails, allowing the caller to fall through.
async fn try_exif_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let config = &ThumbnailConfig::CANONICAL;

    let header = match cook.http_read_at(0, HEADER_SCAN).await {
        Ok(b) => b,
        Err(_) => return,
    };

    let span = find_jpeg_exif_shortcut(&header)
        .map(|i| (i.thumb_file_offset, i.thumb_len, i.source_dims))
        .or_else(|| find_tiff_embedded_jpeg_file_span(&header).map(|(o, l)| (o, l, None)));
    let Some((thumb_offset, thumb_len, source_dims)) = span else { return };

    let embedded = match cook.http_read_at(thumb_offset, thumb_len).await {
        Ok(b) => b,
        Err(_) => return,
    };

    if embedded.len() < 4 || embedded[0] != 0xFF || embedded[1] != 0xD8 { return; }

    let Ok(img) = image::load_from_memory(&embedded) else { return };
    let (thumb_w, thumb_h) = (img.width(), img.height());
    let color_type = img.color();
    let dl_bytes = cook.http_bytes_fetched();

    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);
    cook.http_close().await;

    let (prop_w, prop_h) = source_dims.unwrap_or((thumb_w, thumb_h));
    cook.render_renderer    = Some("shortcut/exif".into());
    cook.render_handler     = RenderHandler::Builtin;
    cook.out_download_bytes = dl_bytes;
    if prop_w > 0 && prop_h > 0 {
        cook.media.properties = Some(image_properties(prop_w, prop_h, color_type));
    }
    cook.render_image = Some(img);
}

// ── Camera-raw JPEG preview shortcut ─────────────────────────────────────────

/// Bytes fetched for a fallback IFD1 range request when IFD1 lies beyond
/// the initial `RAW_HEADER_SCAN` window (e.g. Sony/Adobe DNG with a large
/// XMP block between IFD0 and IFD1).  Covers any realistic thumbnail IFD.
const IFD1_FETCH: usize = 512;

/// Attempt to serve a thumbnail from the embedded JPEG preview in a
/// camera-raw file.
///
/// Covers all TIFF-container raw formats: DNG, CR2, NEF, ARW, ORF, RW2, PEF,
/// SRW, 3FR, MEF, RWL.
///
/// Strategy:
/// - Read the first `RAW_HEADER_SCAN` bytes (32 KiB) — larger than the
///   standard 4 KiB EXIF scan because SubIFD entries can be deeper in raw
///   files.
/// - Traverse the full IFD chain (including SubIFDs) via
///   `find_tiff_embedded_jpeg_file_span`, which picks the **largest** embedded
///   JPEG — typically the full-resolution preview, not the small IFD1 thumbnail.
/// - Issue one Range request for those exact bytes, then close the connection.
///
/// `properties.width/height` reports the sensor resolution from IFD0
/// `ImageWidth`/`ImageLength` when available (native pixel count), falling
/// back to the embedded-preview dimensions for cameras that omit those tags.
async fn try_raw_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let config = &ThumbnailConfig::CANONICAL;

    let header = match cook.http_read_at(0, RAW_HEADER_SCAN).await {
        Ok(b) => b,
        Err(_) => return,
    };

    let span = find_tiff_embedded_jpeg_file_span(&header);
    let (thumb_offset, thumb_len) = if let Some(s) = span {
        s
    } else {
        let Some((little, ifd1_off)) = tiff_endian_and_ifd1_offset(&header) else { return };
        if ifd1_off <= header.len() as u64 { return; }
        let Ok(ifd1_data) = cook.http_fetch_range(ifd1_off, IFD1_FETCH).await else { return };
        let Some((_, _, jpeg_off, jpeg_len)) = parse_tiff_ifd(&ifd1_data, 0, little) else { return };
        match (jpeg_off, jpeg_len) {
            (Some(o), Some(l)) if l >= 4 => (o as u64, l),
            _ => return,
        }
    };

    let embedded = match cook.http_fetch_range(thumb_offset, thumb_len).await {
        Ok(b) => b,
        Err(_) => return,
    };

    if embedded.len() < 4 || embedded[0] != 0xFF || embedded[1] != 0xD8 { return; }

    let Ok(img) = image::load_from_memory(&embedded) else { return };
    let (thumb_w, thumb_h) = (img.width(), img.height());
    let color_type = img.color();
    let dl_bytes = cook.http_bytes_fetched();

    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);
    cook.http_close().await;

    cook.render_renderer    = Some("shortcut/tiff".into());
    cook.render_handler     = RenderHandler::Builtin;
    cook.out_download_bytes = dl_bytes;
    if thumb_w > 0 && thumb_h > 0 {
        cook.media.properties = Some(image_properties(thumb_w, thumb_h, color_type));
    }
    cook.render_image = Some(img);
}

// ── ZIP container shortcut ────────────────────────────────────────────────────

/// Attempt to extract and render a thumbnail from a ZIP-based container.
///
/// Issues **one** Range request (the file tail) that is sized to capture both
/// the Central Directory and the embedded thumbnail data.  If the thumbnail
/// turns out to reside outside the fetched tail the function returns without
/// rendering and the HTTP connection stays open for `deliver`.
async fn try_zip_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let Some((image_bytes, dl_bytes, tail_bytes)) = zip_extract(cook).await else {
        return;
    };

    let config = &ThumbnailConfig::CANONICAL;
    let Ok(img) = image::load_from_memory(&image_bytes) else { return };
    let (src_w, src_h) = (img.width(), img.height());
    let color_type  = img.color();
    let t_render    = Instant::now();
    let img         = pre_scale_to_target(img, config.exact_width, config.exact_height);
    let render_secs = t_render.elapsed().as_secs_f64();

    cook.http_close().await;

    cook.render_renderer         = Some("shortcut/zip".into());
    cook.render_handler          = RenderHandler::Builtin;
    cook.tel_decode_secs         = render_secs;
    cook.out_download_bytes      = dl_bytes;
    cook.tel_download_tail_bytes = tail_bytes;
    if src_w > 0 && src_h > 0 {
        cook.media.properties = Some(image_properties(src_w, src_h, color_type));
    }
    cook.render_image = Some(img);
}

/// Core ZIP extraction logic.  Returns `(image_bytes, total_dl, tail_size)` or
/// `None` when the shortcut cannot be applied.
async fn zip_extract<S: HttpStream>(
    cook: &mut ThumbCook<S>,
) -> Option<(Vec<u8>, u64, u64)> {
    let file_size    = cook.http_stream_len()?;
    let accepts_ranges = cook.http_accepts_ranges;
    if !accepts_ranges || file_size < 22 { return None; }

    let tail_size  = (cook.runtime.shortcut_limits.zip_tail_size as u64).min(file_size) as usize;
    let tail_start = file_size - tail_size as u64;

    let tail = cook.http_fetch_range(tail_start, tail_size).await.ok()?;
    let image_bytes = zip_parse_and_extract(&tail, tail_start)?;
    let dl_bytes    = cook.http_bytes_fetched();
    Some((image_bytes, dl_bytes, tail_size as u64))
}

/// Parse the ZIP tail buffer, locate the thumbnail entry, and return its
/// (decompressed) bytes.  All offsets are translated using `tail_start`.
fn zip_parse_and_extract(tail: &[u8], tail_start: u64) -> Option<Vec<u8>> {
    // ── Locate EOCD ───────────────────────────────────────────────────────
    let eocd = zip_find_eocd(tail)?;
    if eocd + 22 > tail.len() {
        return None;
    }

    // EOCD layout (no comment variant — 22 bytes):
    //   0  PK\x05\x06 (4)
    //   4  disk# (2) | start_disk# (2) | entries_this_disk (2) | entries_total (2)
    //  12  cd_size (4) | cd_offset (4) | comment_len (2)
    let cd_size   = zip_u32(tail, eocd + 12) as usize;
    let cd_offset = zip_u32(tail, eocd + 16) as u64;

    // ── Locate Central Directory in the tail ──────────────────────────────
    if cd_offset < tail_start {
        return None; // CD not captured — thumbnail could be anywhere
    }
    let cd_in_tail = (cd_offset - tail_start) as usize;
    let cd_end     = cd_in_tail.checked_add(cd_size)?;
    if cd_end > tail.len() {
        return None;
    }

    // ── Find thumbnail entry ──────────────────────────────────────────────
    let entry = zip_find_thumb(&tail[cd_in_tail..cd_end])?;

    // ── Locate local file header in the tail ─────────────────────────────
    if entry.local_offset < tail_start {
        return None; // thumbnail data starts before our tail window — give up
    }
    let lh_off = (entry.local_offset - tail_start) as usize;
    if lh_off + 30 > tail.len() {
        return None;
    }
    let lh = &tail[lh_off..];
    if &lh[..4] != b"PK\x03\x04" {
        return None;
    }

    // Local file header layout (30 bytes fixed + variable):
    //   0  PK\x03\x04 (4) | min_ver (2) | flags (2) | method (2)
    //   8  mtime (2) | mdate (2) | crc32 (4)
    //  16  comp_size (4) | uncomp_size (4)
    //  24  — (this is actually at 14,18 but layout below is cumulative from 0)
    // The two variable-length fields at bytes 26–27 and 28–29:
    //  26  fname_len (2) | extra_len (2)
    let fname_len = zip_u16(lh, 26) as usize;
    let extra_len = zip_u16(lh, 28) as usize;

    let data_off = lh_off + 30 + fname_len + extra_len;
    let data_end = data_off.checked_add(entry.comp_size as usize)?;
    if data_end > tail.len() {
        return None; // data extends beyond our tail window
    }

    let compressed = &tail[data_off..data_end];

    // ── Decompress ────────────────────────────────────────────────────────
    match entry.method {
        0 => Some(compressed.to_vec()), // stored — no compression
        8 => zip_inflate(compressed, entry.uncomp_size as usize),
        _ => None, // unsupported compression method
    }
}

// ── ZIP helper types ──────────────────────────────────────────────────────────

struct ZipEntry {
    local_offset: u64,
    comp_size:    u64,
    uncomp_size:  u64,
    method:       u16,
}

// ── ZIP helper functions ──────────────────────────────────────────────────────

/// Find the End-of-Central-Directory record by scanning backwards.
fn zip_find_eocd(buf: &[u8]) -> Option<usize> {
    // EOCD must be within the last 22 + 65535 bytes (max ZIP comment).
    let scan_start = buf.len().saturating_sub(22 + 65535);
    for i in (scan_start..=buf.len().saturating_sub(22)).rev() {
        if buf[i..].starts_with(b"PK\x05\x06") {
            return Some(i);
        }
    }
    None
}

/// Scan a Central Directory buffer for a known thumbnail entry.
fn zip_find_thumb(cd: &[u8]) -> Option<ZipEntry> {
    let mut pos = 0;
    while pos + 46 <= cd.len() {
        if &cd[pos..pos + 4] != b"PK\x01\x02" {
            break;
        }
        // Central directory entry layout (46-byte fixed header):
        //   0  sig (4) | made_by (2) | min_ver (2) | flags (2) | method (2)
        //  10  mtime (2) | mdate (2) | crc32 (4)
        //  20  comp_size (4) | uncomp_size (4)
        //  28  fname_len (2) | extra_len (2) | comment_len (2)
        //  34  disk_start (2) | int_attr (2) | ext_attr (4)
        //  42  local_offset (4)
        //  46  fname … extra … comment
        let method       = zip_u16(cd, pos + 10);
        let comp_size    = zip_u32(cd, pos + 20) as u64;
        let uncomp_size  = zip_u32(cd, pos + 24) as u64;
        let fname_len    = zip_u16(cd, pos + 28) as usize;
        let extra_len    = zip_u16(cd, pos + 30) as usize;
        let comment_len  = zip_u16(cd, pos + 32) as usize;
        let local_offset = zip_u32(cd, pos + 42) as u64;

        let name_end = pos + 46 + fname_len;
        if name_end <= cd.len() {
            let fname = std::str::from_utf8(&cd[pos + 46..name_end]).unwrap_or("");
            if ZIP_THUMB_NAMES.contains(&fname) {
                return Some(ZipEntry { local_offset, comp_size, uncomp_size, method });
            }
        }
        pos += 46 + fname_len + extra_len + comment_len;
    }
    None
}

/// Decompress raw DEFLATE (ZIP method 8) bytes.
fn zip_inflate(data: &[u8], expected_size: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let cap = expected_size.min(64 * 1024 * 1024);
    let mut out = Vec::with_capacity(cap);
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Read a little-endian `u16` from `buf` at `off`.
#[inline]
fn zip_u16(buf: &[u8], off: usize) -> u16 {
    let Some(b) = buf.get(off..off + 2).and_then(|s| <[u8; 2]>::try_from(s).ok()) else {
        return 0;
    };
    u16::from_le_bytes(b)
}

/// Read a little-endian `u32` from `buf` at `off`.
#[inline]
fn zip_u32(buf: &[u8], off: usize) -> u32 {
    let Some(b) = buf.get(off..off + 4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return 0;
    };
    u32::from_le_bytes(b)
}

// ── JPEG: EXIF IFD1 thumbnail ─────────────────────────────────────────────────

/// Location and metadata of an embedded JPEG thumbnail in an EXIF APP1 segment.
struct JpegExifShortcutInfo {
    /// Byte offset within the remote file where the embedded JPEG thumbnail starts.
    thumb_file_offset: u64,
    /// Byte length of the embedded JPEG thumbnail.
    thumb_len: usize,
    /// Full-image pixel dimensions from EXIF IFD0 / ExifIFD, if available.
    source_dims: Option<(u32, u32)>,
}

/// Scan a file prefix (starting at byte 0) for a JPEG APP1/EXIF thumbnail.
/// Returns file-byte span and source dimensions derived from EXIF metadata.
fn find_jpeg_exif_shortcut(bytes: &[u8]) -> Option<JpegExifShortcutInfo> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF { pos += 1; continue; }
        while pos < bytes.len() && bytes[pos] == 0xFF { pos += 1; }
        if pos >= bytes.len() { break; }

        let marker = bytes[pos];
        pos += 1;

        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            if marker == 0xD9 { break; } // EOI
            continue;
        }

        if pos + 2 > bytes.len() { break; }
        let seg_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        if seg_len < 2 { break; }

        if marker == 0xE1 {
            // `pos` is the offset of the APP1 length field.
            // The payload starts at `pos + 2`, which equals its absolute file
            // offset since `bytes` starts at file byte 0.
            let app1_file_offset = (pos + 2) as u64;
            let app1_end = pos + seg_len;
            let app1_data = &bytes[pos + 2..app1_end.min(bytes.len())];
            if let Some(info) = parse_exif_shortcut_info(app1_data, app1_file_offset) {
                return Some(info);
            }
        }

        pos += seg_len;
        if marker == 0xDA { break; } // SOS — stop scanning
    }
    None
}

fn parse_exif_shortcut_info(
    app1_data: &[u8],
    app1_file_offset: u64,
) -> Option<JpegExifShortcutInfo> {
    if app1_data.len() < 6 || &app1_data[0..6] != b"Exif\0\0" { return None; }
    let tiff = &app1_data[6..];
    let tiff_file_offset = app1_file_offset + 6;

    if tiff.len() < 8 { return None; }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(tiff, 2, little)? != 42 { return None; }

    let ifd0_off   = read_u32(tiff, 4, little)? as usize;
    if ifd0_off + 2 > tiff.len() { return None; }
    let ifd0_count = read_u16(tiff, ifd0_off, little)? as usize;

    // Scan IFD0 for ImageWidth/ImageLength and the ExifIFD pointer.
    let mut ifd0_width:   Option<u32>   = None;
    let mut ifd0_height:  Option<u32>   = None;
    let mut exif_ifd_off: Option<usize> = None;
    for i in 0..ifd0_count {
        let entry = ifd0_off + 2 + i * 12;
        if entry + 12 > tiff.len() { break; }
        let tag = read_u16(tiff, entry, little)?;
        let ft  = read_u16(tiff, entry + 2, little)?;
        match tag {
            0x0100 => ifd0_width   = tiff_scalar(tiff, ft, entry + 8, little),
            0x0101 => ifd0_height  = tiff_scalar(tiff, ft, entry + 8, little),
            0x8769 => exif_ifd_off = read_u32(tiff, entry + 8, little).map(|v| v as usize),
            _ => {}
        }
    }

    // Prefer PixelXDimension/PixelYDimension from ExifIFD — these are the
    // authoritative JPEG pixel counts, excluding MCU alignment padding.
    let mut px_width:  Option<u32> = None;
    let mut px_height: Option<u32> = None;
    if let Some(exif_off) = exif_ifd_off {
        if exif_off + 2 <= tiff.len() {
            if let Some(n) = read_u16(tiff, exif_off, little) {
                for i in 0..n as usize {
                    let entry = exif_off + 2 + i * 12;
                    if entry + 12 > tiff.len() { break; }
                    let tag = read_u16(tiff, entry, little).unwrap_or(0);
                    let ft  = read_u16(tiff, entry + 2, little).unwrap_or(0);
                    match tag {
                        0xA002 => px_width  = tiff_scalar(tiff, ft, entry + 8, little),
                        0xA003 => px_height = tiff_scalar(tiff, ft, entry + 8, little),
                        _ => {}
                    }
                }
            }
        }
    }

    let source_dims = match (px_width.or(ifd0_width), px_height.or(ifd0_height)) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    };

    // IFD1 holds the embedded thumbnail.
    let ifd1_ptr_off = ifd0_off + 2 + ifd0_count * 12;
    let ifd1_off     = read_u32(tiff, ifd1_ptr_off, little)? as usize;
    if ifd1_off == 0 || ifd1_off + 2 > tiff.len() { return None; }

    let ifd1_count = read_u16(tiff, ifd1_off, little)? as usize;
    let mut jpeg_off: Option<u64>   = None;
    let mut jpeg_len: Option<usize> = None;
    for i in 0..ifd1_count {
        let entry = ifd1_off + 2 + i * 12;
        if entry + 12 > tiff.len() { break; }
        let tag = read_u16(tiff, entry, little)?;
        match tag {
            0x0201 => jpeg_off = read_u32(tiff, entry + 8, little).map(|v| v as u64),
            0x0202 => jpeg_len = read_u32(tiff, entry + 8, little).map(|v| v as usize),
            _ => {}
        }
    }

    let off = jpeg_off?;
    let len = jpeg_len?;
    if len < 4 { return None; }

    Some(JpegExifShortcutInfo {
        thumb_file_offset: tiff_file_offset + off,
        thumb_len: len,
        source_dims,
    })
}

/// Read a SHORT (3) or LONG (4) TIFF field value as `u32`.
fn tiff_scalar(bytes: &[u8], field_type: u16, offset: usize, little: bool) -> Option<u32> {
    match field_type {
        3 => read_u16(bytes, offset, little).map(|v| v as u32), // SHORT
        4 => read_u32(bytes, offset, little),                    // LONG
        _ => None,
    }
}

// ── TIFF: embedded JPEG preview ───────────────────────────────────────────────

/// Return the `(file_byte_offset, byte_length)` of the best embedded JPEG
/// thumbnail found by traversing the TIFF IFD chain from the file prefix.
fn find_tiff_embedded_jpeg_file_span(bytes: &[u8]) -> Option<(u64, usize)> {
    let (off, len) = find_tiff_embedded_jpeg_span(bytes)?;
    Some((off as u64, len))
}

/// Return `(little_endian, ifd1_file_offset)` by reading IFD0's next-IFD
/// pointer from the TIFF header.
///
/// Used as a fallback when `find_tiff_embedded_jpeg_file_span` returns `None`
/// because IFD1 lies beyond the initial scan window.
fn tiff_endian_and_ifd1_offset(bytes: &[u8]) -> Option<(bool, u64)> {
    if bytes.len() < 8 { return None; }
    let little = match &bytes[0..2] { b"II" => true, b"MM" => false, _ => return None };
    if read_u16(bytes, 2, little)? != 42 { return None; }
    let ifd0_off = read_u32(bytes, 4, little)? as usize;
    if ifd0_off + 2 > bytes.len() { return None; }
    let count = read_u16(bytes, ifd0_off, little)? as usize;
    let next_ptr_off = ifd0_off.checked_add(2)?.checked_add(count.checked_mul(12)?)?;
    if next_ptr_off + 4 > bytes.len() { return None; }
    let ifd1_off = read_u32(bytes, next_ptr_off, little)?;
    if ifd1_off == 0 { return None; }
    Some((little, ifd1_off as u64))
}

fn find_tiff_embedded_jpeg_span(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() < 8 { return None; }
    let little = match &bytes[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(bytes, 2, little)? != 42 { return None; } // classic TIFF magic

    let ifd0_off = read_u32(bytes, 4, little)? as usize;
    if ifd0_off + 2 > bytes.len() { return None; }

    let mut queue = VecDeque::from([ifd0_off]);
    let mut visited = HashSet::new();
    let mut best: Option<(usize, usize)> = None;

    while let Some(ifd_off) = queue.pop_front() {
        if !visited.insert(ifd_off) { continue; }
        let Some((next, subs, joff, jlen)) = parse_tiff_ifd(bytes, ifd_off, little) else { continue };

        if next != 0 && next + 2 <= bytes.len() { queue.push_back(next); }
        for s in subs { if s != 0 && s + 2 <= bytes.len() { queue.push_back(s); } }

        if let (Some(o), Some(l)) = (joff, jlen) {
            if l >= 4 && best.is_none_or(|(_, bl)| l > bl) {
                best = Some((o, l));
            }
        }
    }
    best
}

fn parse_tiff_ifd(
    bytes: &[u8],
    ifd_off: usize,
    little: bool,
) -> Option<(usize, Vec<usize>, Option<usize>, Option<usize>)> {
    if ifd_off + 2 > bytes.len() { return None; }
    let count = read_u16(bytes, ifd_off, little)? as usize;
    let entries_off = ifd_off + 2;
    let next_ptr_off = entries_off.checked_add(count.checked_mul(12)?)?;
    if next_ptr_off + 4 > bytes.len() { return None; }

    let mut jpeg_off: Option<usize> = None;
    let mut jpeg_len: Option<usize> = None;
    let mut sub_ifds: Vec<usize> = Vec::new();
    let mut compression: Option<u16> = None;
    let mut strip_off: Option<usize> = None;
    let mut strip_cnt: Option<usize> = None;
    let mut tile_off: Option<usize> = None;
    let mut tile_cnt: Option<usize> = None;

    for i in 0..count {
        let entry = entries_off + i * 12;
        if entry + 12 > bytes.len() { break; }
        let tag = read_u16(bytes, entry, little)?;
        let ft = read_u16(bytes, entry + 2, little)?;
        let fc = read_u32(bytes, entry + 4, little)? as usize;
        let v = read_u32(bytes, entry + 8, little)? as usize;

        match tag {
            0x0103 => compression = Some(v as u16),
            0x0201 => jpeg_off = Some(v),
            0x0202 => jpeg_len = Some(v),
            0x0111 => strip_off = tiff_u32_values(bytes, ft, fc, v, little, 1).first().copied().map(|x| x as usize),
            0x0117 => strip_cnt = tiff_u32_values(bytes, ft, fc, v, little, 1).first().copied().map(|x| x as usize),
            0x0144 => tile_off  = tiff_u32_values(bytes, ft, fc, v, little, 1).first().copied().map(|x| x as usize),
            0x0145 => tile_cnt  = tiff_u32_values(bytes, ft, fc, v, little, 1).first().copied().map(|x| x as usize),
            0x014A => sub_ifds.extend(tiff_subifd_offsets(bytes, ft, fc, v, little)),
            _ => {}
        }
    }

    // JPEG-compressed IFDs may use strip/tile layout instead of JPEGInterchangeFormat.
    if jpeg_off.is_none() || jpeg_len.is_none() {
        if matches!(compression, Some(6 | 7)) {
            if let (Some(o), Some(l)) = (strip_off, strip_cnt) { jpeg_off = Some(o); jpeg_len = Some(l); }
            else if let (Some(o), Some(l)) = (tile_off, tile_cnt) { jpeg_off = Some(o); jpeg_len = Some(l); }
        }
    }

    let next_ifd = read_u32(bytes, next_ptr_off, little)? as usize;
    Some((next_ifd, sub_ifds, jpeg_off, jpeg_len))
}

fn tiff_subifd_offsets(bytes: &[u8], ft: u16, fc: usize, v: usize, little: bool) -> Vec<usize> {
    if fc == 0 || ft != 4 { return Vec::new(); }
    if fc == 1 { return vec![v]; }
    (0..fc.min(32))
        .filter_map(|i| read_u32(bytes, v + i * 4, little).map(|x| x as usize))
        .collect()
}

fn tiff_u32_values(bytes: &[u8], ft: u16, fc: usize, v: usize, little: bool, max: usize) -> Vec<u32> {
    let count = fc.min(max);
    if count == 0 { return Vec::new(); }
    match ft {
        3 => { // SHORT
            if fc == 1 { vec![v as u32] }
            else { (0..count).filter_map(|i| read_u16(bytes, v + i * 2, little).map(|x| x as u32)).collect() }
        }
        4 => { // LONG
            if fc == 1 { vec![v as u32] }
            else { (0..count).filter_map(|i| read_u32(bytes, v + i * 4, little)).collect() }
        }
        _ => Vec::new(),
    }
}

// ── Power-of-two pre-scale ──────────────────────────────────────────────────

/// Reduce a decoded image to near the thumbnail target using a two-phase
/// strategy that balances speed and quality:
///
/// **Phase 1** — fast 2×2 box-average halvings down to ~2× the target.
/// Each step averages four source pixels into one, done in-place on the raw
/// pixel buffer.  The write cursor (`oy*(w/2)+ox`) is always ≤ the first
/// read cursor (`2*oy*w+2*ox`), so no extra allocation is needed and we
/// never overwrite source data before it has been read.
///
/// **Phase 2** — Triangle filter for the remaining ≤2× step.
/// At ≤2× the Triangle kernel is small, so cost is low, but the quality
/// improvement over a final nearest-neighbour step is substantial.
fn pre_scale_to_target(img: DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let (tw, th) = (target_w.max(1), target_h.max(1));

    // Phase 1: in-place 2×2 box-average halvings down to ≈2× target.
    let mut divisor = 1u32;
    loop {
        let next = divisor * 2;
        if w / next >= tw * 2 && h / next >= th * 2 { divisor = next; } else { break; }
    }
    let img = if divisor > 1 {
        let steps = divisor.trailing_zeros();
        let mut img = img;
        for _ in 0..steps { img = box_half(img); }
        img
    } else {
        img
    };

    // Phase 2: Triangle filter for the ≤2× remainder.
    let (sw, sh) = (img.width(), img.height());
    let scale = (tw as f32 / sw as f32).max(th as f32 / sh as f32);
    if scale >= 1.0 {
        return img; // already at or below target size
    }
    let out_w = ((sw as f32 * scale).ceil() as u32).max(tw);
    let out_h = ((sh as f32 * scale).ceil() as u32).max(th);
    img.resize_exact(out_w, out_h, FilterType::Triangle)
}

/// Halve an image in both dimensions by averaging each 2×2 block of pixels.
///
/// Operates in-place on the raw pixel buffer: the output pixel at `(ox, oy)`
/// is always written to a buffer position ≤ the first source pixel it reads
/// (`oy*(w/2)+ox ≤ 2*oy*w+2*ox`), so source data is never overwritten before
/// it has been consumed.
///
/// Odd dimensions are truncated (last row/column dropped) — acceptable for a
/// pre-scale pass where the final fill_crop will trim any small error.
fn box_half(img: DynamicImage) -> DynamicImage {
    use image::{GrayImage, RgbImage, RgbaImage};

    let (w, h) = (img.width(), img.height());
    let ow = w / 2;
    let oh = h / 2;
    if ow == 0 || oh == 0 { return img; }

    // Luma8 — 1 byte/pixel
    if matches!(img, DynamicImage::ImageLuma8(_)) {
        let mut buf = img.into_luma8().into_raw();
        let stride = w as usize;
        for oy in 0..oh as usize {
            let row0 = oy * 2 * stride;
            let row1 = row0 + stride;
            let dst  = oy * ow as usize;
            for ox in 0..ow as usize {
                let sx = ox * 2;
                buf[dst + ox] = ((buf[row0+sx] as u16 + buf[row0+sx+1] as u16
                                + buf[row1+sx] as u16 + buf[row1+sx+1] as u16 + 2) >> 2) as u8;
            }
        }
        buf.truncate((ow * oh) as usize);
        return DynamicImage::ImageLuma8(GrayImage::from_raw(ow, oh, buf).unwrap());
    }

    // RGBA8 — 4 bytes/pixel
    if img.color().has_alpha() {
        let mut buf = img.into_rgba8().into_raw();
        let stride = (w * 4) as usize;
        for oy in 0..oh as usize {
            let row0 = oy * 2 * stride;
            let row1 = row0 + stride;
            let dst  = oy * ow as usize * 4;
            for ox in 0..ow as usize {
                let sx = ox * 8; // ox * 2 pixels * 4 channels
                for c in 0..4usize {
                    buf[dst + ox*4 + c] = ((buf[row0+sx+c]   as u16 + buf[row0+sx+4+c] as u16
                                          + buf[row1+sx+c]   as u16 + buf[row1+sx+4+c] as u16
                                          + 2) >> 2) as u8;
                }
            }
        }
        buf.truncate((ow * oh * 4) as usize);
        return DynamicImage::ImageRgba8(RgbaImage::from_raw(ow, oh, buf).unwrap());
    }

    // RGB8 — 3 bytes/pixel (most photos land here)
    let mut buf = img.into_rgb8().into_raw();
    let stride = (w * 3) as usize;
    for oy in 0..oh as usize {
        let row0 = oy * 2 * stride;
        let row1 = row0 + stride;
        let dst  = oy * ow as usize * 3;
        for ox in 0..ow as usize {
            let sx = ox * 6; // ox * 2 pixels * 3 channels
            for c in 0..3usize {
                buf[dst + ox*3 + c] = ((buf[row0+sx+c]   as u16 + buf[row0+sx+3+c] as u16
                                      + buf[row1+sx+c]   as u16 + buf[row1+sx+3+c] as u16
                                      + 2) >> 2) as u8;
            }
        }
    }
    buf.truncate((ow * oh * 3) as usize);
    DynamicImage::ImageRgb8(RgbImage::from_raw(ow, oh, buf).unwrap())
}


/// Build the `properties` JSON object for a decoded image.
///
/// Uses the *source* dimensions (`src_w`/`src_h`) so the reported size reflects
/// the original file, not any intermediate scaling done before this call.
/// `color_type` should be captured from `img.color()` before any pre-scaling.
fn image_properties(src_w: u32, src_h: u32, color_type: image::ColorType) -> serde_json::Value {
    let ch = color_type.channel_count() as u32;
    let color_depth = if ch > 0 { color_type.bits_per_pixel() as u32 / ch } else { color_type.bits_per_pixel() as u32 };
    serde_json::json!({
        "width":       src_w,
        "height":      src_h,
        "color_depth": color_depth,
    })
}

// ── Byte readers ─────────────────────────────────────────────────────────────

fn read_u16(buf: &[u8], off: usize, little: bool) -> Option<u16> {
    let b0 = *buf.get(off)?;
    let b1 = *buf.get(off + 1)?;
    Some(if little { u16::from_le_bytes([b0, b1]) } else { u16::from_be_bytes([b0, b1]) })
}

fn read_u32(buf: &[u8], off: usize, little: bool) -> Option<u32> {
    let b0 = *buf.get(off)?;
    let b1 = *buf.get(off + 1)?;
    let b2 = *buf.get(off + 2)?;
    let b3 = *buf.get(off + 3)?;
    Some(if little { u32::from_le_bytes([b0, b1, b2, b3]) } else { u32::from_be_bytes([b0, b1, b2, b3]) })
}

// ── Public pipeline step (merged) ────────────────────────────────────────────

// SMALL_FILE_THRESHOLD is now runtime-configurable via
// cook.runtime.shortcut_limits.small_file_threshold.
// See spec::ShortcutLimits for tier-specific values.

/// Try to produce a thumbnail without a full upstream decode.
///
/// Five active paths, in priority order:
///
/// 1. **Small image** (any `image`-supported format ≤ `SMALL_FILE_THRESHOLD`) —
///    read all bytes, full decode, pre-scale, set `cook.render_image`.
/// 2. **EXIF embedded thumbnail** (JPEG EXIF IFD1 / TIFF IFD chain) —
///    two-phase read: header scan then targeted Range for the embedded JPEG.
/// 3. **Progressive JPEG** — read a heuristic byte budget, decode the first
///    progressive scan, set `cook.render_image`.
/// 4. **Camera-raw preview** (DNG, CR2, NEF, ARW, …) — 32 KiB header scan,
///    full IFD traversal, targeted Range for the largest embedded JPEG.
/// 5. **ZIP container** (ODT, DOCX, …) — single tail Range fetch.
///
/// Sets `cook.render_image = Some(img)` and closes the HTTP connection on success.
/// Leaves both untouched on any failure so the caller can fall through to
/// the handoff path.
pub async fn shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let config = &ThumbnailConfig::CANONICAL;

    let ext_owned = cook.media.extension.clone().unwrap_or_default();
    let ext = ext_owned.as_str();

    // ── EXIF embedded thumbnail (JPEG EXIF IFD1 / TIFF IFD chain) ────────
    //
    // Attempt this BEFORE the small-image full-download path so that JPEG/TIFF
    // files with an embedded thumbnail are served from the thumbnail bytes only
    // (typically a few hundred bytes header + a few KB for the thumbnail) rather
    // than downloading the entire source file.
    let is_jpeg = matches!(ext, "jpeg");
    let is_tiff = matches!(ext, "tiff");
    if is_jpeg || is_tiff {
        try_exif_shortcut(cook).await;
        if !cook.http_is_open() { return; }
    }

    // ── Small image: any format `image` supports, ≤ SMALL_FILE_THRESHOLD ─
    //
    // `inspect` has already classified the file; trust its verdict.
    // Covers JPEG, PNG, GIF, BMP, WebP, TIFF, ICO, … without any byte sniffing.
    //
    // Keep this BEFORE the progressive JPEG path so genuinely small files
    // always use a full decode for reliability/quality.
    //
    // Exclude formats that require libav (HEIC, HEIF, AVIF, EXR, HDR) — they
    // are classified as `FileKind::Image` but `image::load_from_memory` cannot
    // decode them.  If we entered streaming mode and consumed the bytes here,
    // the connection would be at EOF when tier 2 tries `take_reader()` for libav.
    let is_image_crate_format = !matches!(ext, "heic" | "heif" | "avif" | "exr" | "hdr");
    let is_small_image = cook.media.kind == Some(FileKind::Image)
        && is_image_crate_format
        && cook.http_stream_len()
            .map(|n| n <= cook.runtime.shortcut_limits.small_file_threshold)
            .unwrap_or(false);

    if is_small_image {
        let file_size = cook.http_stream_len().unwrap_or(0);

        cook.http_rewind();
        cook.http_enter_streaming_mode();

        let mut data = Vec::with_capacity(file_size as usize);
        {
            let mut tmp = vec![0u8; 8 * 1024];
            loop {
                match cook.http_read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => data.extend_from_slice(&tmp[..n]),
                    Err(_) => { data.clear(); break; }
                }
            }
        }

        if !data.is_empty() {
            if let Ok(img) = image::load_from_memory(&data) {
                let (src_w, src_h) = (img.width(), img.height());
                let color_type     = img.color();
                let dl_bytes = cook.http_bytes_fetched();

                cook.http_close().await;

                let img = pre_scale_to_target(img, config.exact_width, config.exact_height);

                cook.render_renderer   = Some("shortcut/small".into());
                cook.render_handler    = RenderHandler::Builtin;
                cook.out_download_bytes = dl_bytes;
                if src_w > 0 && src_h > 0 {
                    cook.media.properties = Some(image_properties(src_w, src_h, color_type));
                }
                cook.render_image = Some(img);
                return;
            }
            // load_from_memory failed — close and fall through to defer.
            cook.http_close().await;
        }
    }

    // ── Progressive JPEG (falls through from EXIF and small-image checks) ─
    if is_jpeg {
        try_progressive_jpeg_shortcut(cook).await;
        if !cook.http_is_open() { return; }
    }

    // ── Camera-raw embedded JPEG preview ─────────────────────────────────
    let is_raw = cook.media.kind == Some(FileKind::Image)
        && matches!(cook.media.extension.as_deref(),
            Some("dng" | "cr2" | "nef" | "arw" | "orf" | "rw2"
               | "pef" | "srw" | "3fr" | "mef" | "rwl"));
    if is_raw {
        try_raw_shortcut(cook).await;
        if !cook.http_is_open() { return; }
    }

    // ── ZIP containers (ODT, DOCX, …) ────────────────────────────────────
    let is_zip_doc = cook.media.kind == Some(FileKind::Document)
        && matches!(cook.media.extension.as_deref(),
            Some("docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp"));
    if is_zip_doc { try_zip_shortcut(cook).await; }
}
