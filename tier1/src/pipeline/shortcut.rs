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
///
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
    "Thumbnails/thumbnail.png", // ODT / ODS / ODP (ODF family)
    "docProps/thumbnail.jpeg",  // DOCX / XLSX / PPTX (OOXML family)
    "docProps/thumbnail.jpg",   // OOXML variant
    "docProps/thumbnail.png",   // OOXML variant
];

//  Progressive JPEG shortcut

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
    let estimated = w
        .zip(h)
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
    let color_type = img.color();
    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);
    let dl_bytes = cook.http_bytes_fetched();

    cook.render_renderer = Some("shortcut/progressive".into());
    cook.render_handler = RenderHandler::Builtin;
    cook.tel_decode_secs = decode_secs;
    cook.out_download_bytes = dl_bytes;
    cook.render_is_progressive_partial = true; // Mark as partial decode to suppress pixel-art heuristic
    if src_w > 0 && src_h > 0 {
        cook.media.properties = Some(image_properties(src_w, src_h, color_type));
    }

    cook.http_close().await;
    cook.render_image = Some(img);
}

//  EXIF embedded thumbnail shortcut 

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
        .or_else(|| find_tiff_embedded_jpeg_file_span(&header).map(|(o, l)| (o, l, None)))
        .or_else(|| {
            find_png_exif_shortcut(&header).map(|i| (i.thumb_file_offset, i.thumb_len, i.source_dims))
        });
    let Some((thumb_offset, thumb_len, source_dims)) = span else {
        return;
    };

    let embedded = match cook.http_read_at(thumb_offset, thumb_len).await {
        Ok(b) => b,
        Err(_) => return,
    };

    if embedded.len() < 4 || embedded[0] != 0xFF || embedded[1] != 0xD8 {
        return;
    }

    // When EXIF IFD0/ExifIFD don't have dimension tags, try to locate the
    // real JPEG SOF marker.  First attempt: walk APP segments in the cached
    // 4 KB header and read 512 bytes starting after the last APP.  If the
    // header scan falls off the end (APP segments extend beyond 4 KB), fall
    // back to reading up to 64 KB from offset 0.
    let source_dims: Option<(u32, u32)> = if let Some(dims) = source_dims {
        Some(dims)
    } else if let Some(sof_offset) = super::inspect::jpeg_app_segments_end(&header) {
        cook.http_read_at(sof_offset, 512)
            .await
            .ok()
            .and_then(|chunk| super::inspect::find_sof_in_bytes(&chunk))
            .map(|(w, h, _)| (w, h))
    } else {
        // Header scan couldn't find the SOF boundary — read a larger prefix.
        let Ok(big) = cook.http_read_at(0, 65536).await else {
            return;
        };
        super::inspect::jpeg_sof_dimensions(&big).map(|(w, h, _)| (w, h))
    };
    let Some((prop_w, prop_h)) = source_dims else {
        return;
    };

    let Ok(img) = image::load_from_memory(&embedded) else {
        return;
    };
    let color_type = img.color();
    let dl_bytes = cook.http_bytes_fetched();

    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);
    cook.http_close().await;

    cook.render_renderer = Some("shortcut/exif".into());
    cook.render_handler = RenderHandler::Builtin;
    cook.out_download_bytes = dl_bytes;
    if prop_w > 0 && prop_h > 0 {
        cook.media.properties = Some(image_properties(prop_w, prop_h, color_type));
    }
    cook.render_image = Some(img);
}

//  Camera-raw JPEG preview shortcut

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
/// `properties.width_pixels`/`height_pixels` reports the sensor resolution from IFD0
/// `ImageWidth`/`ImageLength` when available (native pixel count), falling
/// back to the embedded-preview dimensions for cameras that omit those tags.
async fn try_raw_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let config = &ThumbnailConfig::CANONICAL;

    let header = match cook.http_read_at(0, RAW_HEADER_SCAN).await {
        Ok(b) => b,
        Err(_) => return,
    };

    // Determine endianness up-front — used for the fallback span search and
    // for the orientation correction that follows decoding.
    let little = match header.get(0..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return,
    };

    let span = find_tiff_embedded_jpeg_file_span(&header);
    let (thumb_offset, thumb_len) = if let Some(s) = span {
        s
    } else {
        // SubIFD fallback (DNG/NEF/ARW/…): the large preview JPEG lives in a
        // SubIFD (tag 0x014A) whose IFD table typically lies well past the
        // RAW_HEADER_SCAN window.  Parse IFD0 to collect those offsets, then
        // stream forward on the open connection to read each SubIFD table.
        let ifd0_off = match read_u32(&header, 4, little) {
            Some(v) => v as usize,
            None => return,
        };
        let subifd_span: Option<(u64, usize)> =
            if let Some((_, sub_ifds, _, _)) = parse_tiff_ifd(&header, ifd0_off, little) {
                let mut best: Option<(u64, usize)> = None;
                for sub_off in sub_ifds {
                    if sub_off + 2 <= header.len() {
                        continue;
                    } // already covered
                    let Ok(sub_data) = cook.http_read_at(sub_off as u64, IFD1_FETCH).await else {
                        continue;
                    };
                    let Some((_, _, jpeg_off, jpeg_len)) = parse_tiff_ifd(&sub_data, 0, little) else {
                        continue;
                    };
                    if let (Some(o), Some(l)) = (jpeg_off, jpeg_len)
                        && l >= 4 && best.is_none_or(|(_, bl)| l > bl) {
                            best = Some((o as u64, l));
                        }
                }
                best
            } else {
                None
            };

        if let Some(s) = subifd_span {
            s
        } else {
            // IFD1 fallback: regular TIFFs (and some CR2/NEF) store the
            // embedded thumbnail in IFD1 using JPEGInterchangeFormat.
            let Some((_, ifd1_off)) = tiff_endian_and_ifd1_offset(&header) else {
                return;
            };
            if ifd1_off <= header.len() as u64 {
                return;
            }
            let Ok(ifd1_data) = cook.http_read_at(ifd1_off, IFD1_FETCH).await else {
                return;
            };
            let Some((_, _, jpeg_off, jpeg_len)) = parse_tiff_ifd(&ifd1_data, 0, little) else {
                return;
            };
            match (jpeg_off, jpeg_len) {
                (Some(o), Some(l)) if l >= 4 => (o as u64, l),
                _ => return,
            }
        }
    };

    let embedded = match cook.http_read_at(thumb_offset, thumb_len).await {
        Ok(b) => b,
        Err(_) => return,
    };

    if embedded.len() < 4 || embedded[0] != 0xFF || embedded[1] != 0xD8 {
        return;
    }

    let Ok(img) = image::load_from_memory(&embedded) else {
        return;
    };
    let color_type = img.color();
    let dl_bytes = cook.http_bytes_fetched();

    // Apply TIFF IFD0 Orientation correction.  The embedded JPEG preview is
    // physically stored in the sensor's capture orientation; the Orientation
    // tag (0x0112) says how to rotate it for correct display.
    let img = match tiff_ifd0_orientation(&header, little) {
        3 => img.rotate180(),
        6 => img.rotate90(),  // right-top: rotate 90° CW to display
        8 => img.rotate270(), // left-bottom: rotate 90° CCW to display
        _ => img,
    };
    let (thumb_w, thumb_h) = (img.width(), img.height());

    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);
    cook.http_close().await;

    cook.render_renderer = Some("shortcut/tiff".into());
    cook.render_handler = RenderHandler::Builtin;
    cook.out_download_bytes = dl_bytes;
    // Prefer IFD0 sensor resolution over the embedded preview dimensions.
    if let Some((sw, sh)) = tiff_ifd0_dimensions(&header, little) {
        // Raw sensor bpp — TIFF IFD0 BitsPerSample tag (0x0102) is per-component,
        // typically 12 or 14 for raw files.  We don't parse it here yet, so omit bpp.
        cook.media.properties = Some(serde_json::json!({
            "width": sw,
            "height": sh,
            "lossless": 1,
        }));
    } else if thumb_w > 0 && thumb_h > 0 {
        cook.media.properties = Some(image_properties(thumb_w, thumb_h, color_type));
    }
    cook.render_image = Some(img);
}

//  ZIP container shortcut 

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
    let Ok(img) = image::load_from_memory(&image_bytes) else {
        return;
    };
    let (src_w, src_h) = (img.width(), img.height());
    let color_type = img.color();
    let t_render = Instant::now();
    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);
    let render_secs = t_render.elapsed().as_secs_f64();

    cook.http_close().await;

    cook.render_renderer = Some("shortcut/zip".into());
    cook.render_handler = RenderHandler::Builtin;
    cook.tel_decode_secs = render_secs;
    cook.out_download_bytes = dl_bytes;
    cook.tel_download_tail_bytes = tail_bytes;
    // Document thumbnails are just previews — the source file isn't an image
    // so reporting the thumbnail's pixel dimensions as "properties" is misleading.
    if cook.media.kind != Some(FileKind::Document) && src_w > 0 && src_h > 0 {
        cook.media.properties = Some(image_properties(src_w, src_h, color_type));
    }
    cook.render_image = Some(img);
}

/// Core ZIP extraction logic.  Returns `(image_bytes, total_dl, tail_size)` or
/// `None` when the shortcut cannot be applied.
///
/// Phase 1: fetch the tail (Central Directory) via a Range request so we can
/// discover where the thumbnail lives.  Phase 2: if the thumbnail sits inside
/// the tail window, extract it directly (zero extra I/O).  If it is outside
/// the tail, read it via the existing HttpBuffer's `read_at` — this streams
/// through the already-open connection on tier 2 rather than opening a third
/// Range request.  Tier 1 gives up here (the out-of-tail path is guarded by
/// `zip_tail_size`, which is small on tier 1 — if the thumbnail was far enough
/// to miss the tail, tier 1 hands off to tier 2).
async fn zip_extract<S: HttpStream>(cook: &mut ThumbCook<S>) -> Option<(Vec<u8>, u64, u64)> {
    let file_size = cook.http_stream_len()?;
    let accepts_ranges = cook.http_accepts_ranges;
    if file_size < 22 {
        return None;
    }

    let tail_size = (cook.runtime.shortcut_limits.zip_tail_size as u64).min(file_size) as usize;
    let tail_start = file_size - tail_size as u64;

    // When the tail window covers the entire file, reuse the already-open
    // streaming connection rather than opening a redundant Range request.
    let tail = if tail_start == 0 {
        cook.http_read_at(0, file_size as usize).await.ok()?
    } else {
        if !accepts_ranges {
            return None;
        }
        cook.http_fetch_range(tail_start, tail_size).await.ok()?
    };

    // Phase 1: find the thumbnail entry from the Central Directory.
    let entry = zip_find_thumb_entry(&tail, tail_start)?;

    let image_bytes = if entry.local_offset >= tail_start {
        // Thumbnail data sits within the tail window — extract inline,
        // zero extra I/O.
        zip_extract_from_buffer(&tail, &entry, tail_start)?
    } else {
        // Thumbnail is outside the tail window.  Read it through the
        // existing HttpBuffer (`read_at`) rather than opening a third
        // Range request.  The buffer streams from wherever it is now
        // (typically still near the front from inspect) and pages the
        // data into its cache.  IO time is free on Cloudflare Workers
        // and continues the already-open connection on tier 2.
        //
        // Tier 1's small `zip_tail_size` (128 KiB) means this path is
        // only reached when the thumbnail is far from the tail — in
        // practice tier 1 gives up here (the read would exceed the CPU
        // budget) and hands off to tier 2, where the buffer is already
        // open and streaming is cheap.
        let fetch_size = (entry.comp_size as u64).saturating_add(512); // local header + filename + extra overhead
        let lh_data = cook.http_read_at(entry.local_offset, fetch_size as usize).await.ok()?;
        zip_extract_from_buffer(&lh_data, &entry, entry.local_offset)?
    };

    let dl_bytes = cook.http_bytes_fetched();
    Some((image_bytes, dl_bytes, tail_size as u64))
}

/// Find the thumbnail entry from a ZIP tail buffer.  Returns the entry metadata
/// (local offset, sizes, method) or `None` if no thumbnail is found or the CD
/// is incomplete.
fn zip_find_thumb_entry(tail: &[u8], tail_start: u64) -> Option<ZipEntry> {
    let eocd = zip_find_eocd(tail)?;
    if eocd + 22 > tail.len() {
        return None;
    }

    let cd_size = zip_u32(tail, eocd + 12) as usize;
    let cd_offset = zip_u32(tail, eocd + 16) as u64;

    if cd_offset < tail_start {
        return None;
    }
    let cd_in_tail = (cd_offset - tail_start) as usize;
    let cd_end = cd_in_tail.checked_add(cd_size)?;
    if cd_end > tail.len() {
        return None;
    }

    zip_find_thumb(&tail[cd_in_tail..cd_end])
}

/// Extract and decompress thumbnail data from a buffer that contains the local
/// file header + compressed data, starting at `buf_start` (absolute file offset
/// of the first byte in `buf`).
fn zip_extract_from_buffer(buf: &[u8], entry: &ZipEntry, buf_start: u64) -> Option<Vec<u8>> {
    if entry.local_offset < buf_start {
        return None;
    }
    let lh_off = (entry.local_offset - buf_start) as usize;
    if lh_off + 30 > buf.len() {
        return None;
    }
    let lh = &buf[lh_off..];
    if &lh[..4] != b"PK\x03\x04" {
        return None;
    }

    let fname_len = zip_u16(lh, 26) as usize;
    let extra_len = zip_u16(lh, 28) as usize;

    let data_off = lh_off + 30 + fname_len + extra_len;
    let data_end = data_off.checked_add(entry.comp_size as usize)?;
    if data_end > buf.len() {
        return None;
    }

    let compressed = &buf[data_off..data_end];

    match entry.method {
        0 => Some(compressed.to_vec()),
        8 => zip_inflate(compressed, entry.uncomp_size as usize),
        _ => None,
    }
}

//  ZIP helper types 

struct ZipEntry {
    local_offset: u64,
    comp_size: u64,
    uncomp_size: u64,
    method: u16,
}

//  ZIP helper functions 

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
        let method = zip_u16(cd, pos + 10);
        let comp_size = zip_u32(cd, pos + 20) as u64;
        let uncomp_size = zip_u32(cd, pos + 24) as u64;
        let fname_len = zip_u16(cd, pos + 28) as usize;
        let extra_len = zip_u16(cd, pos + 30) as usize;
        let comment_len = zip_u16(cd, pos + 32) as usize;
        let local_offset = zip_u32(cd, pos + 42) as u64;

        let name_end = pos + 46 + fname_len;
        if name_end <= cd.len() {
            let fname = std::str::from_utf8(&cd[pos + 46..name_end]).unwrap_or("");
            if ZIP_THUMB_NAMES.contains(&fname) {
                return Some(ZipEntry {
                    local_offset,
                    comp_size,
                    uncomp_size,
                    method,
                });
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

//  JPEG: EXIF IFD1 thumbnail

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
        if bytes[pos] != 0xFF {
            pos += 1;
            continue;
        }
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        let marker = bytes[pos];
        pos += 1;

        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            if marker == 0xD9 {
                break;
            } // EOI
            continue;
        }

        if pos + 2 > bytes.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        if seg_len < 2 {
            break;
        }

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
        if marker == 0xDA {
            break;
        } // SOS — stop scanning
    }
    None
}

fn parse_exif_shortcut_info(app1_data: &[u8], app1_file_offset: u64) -> Option<JpegExifShortcutInfo> {
    if app1_data.len() < 6 || &app1_data[0..6] != b"Exif\0\0" {
        return None;
    }
    parse_tiff_exif_thumbnail(&app1_data[6..], app1_file_offset + 6)
}

/// Parse an embedded JPEG thumbnail from raw TIFF data (no `Exif\0\0` prefix).
///
/// Used by both the JPEG/TIFF EXIF path (which strips the APP1 `Exif\0\0`
/// header) and the PNG `eXIf` chunk path (which stores raw TIFF directly).
fn parse_tiff_exif_thumbnail(tiff: &[u8], tiff_file_offset: u64) -> Option<JpegExifShortcutInfo> {
    if tiff.len() < 8 {
        return None;
    }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(tiff, 2, little)? != 42 {
        return None;
    }

    let ifd0_off = read_u32(tiff, 4, little)? as usize;
    if ifd0_off + 2 > tiff.len() {
        return None;
    }
    let ifd0_count = read_u16(tiff, ifd0_off, little)? as usize;

    // Scan IFD0 for ImageWidth/ImageLength and the ExifIFD pointer.
    let mut ifd0_width: Option<u32> = None;
    let mut ifd0_height: Option<u32> = None;
    let mut exif_ifd_off: Option<usize> = None;
    for i in 0..ifd0_count {
        let entry = ifd0_off + 2 + i * 12;
        if entry + 12 > tiff.len() {
            break;
        }
        let tag = read_u16(tiff, entry, little)?;
        let ft = read_u16(tiff, entry + 2, little)?;
        match tag {
            0x0100 => ifd0_width = tiff_scalar(tiff, ft, entry + 8, little),
            0x0101 => ifd0_height = tiff_scalar(tiff, ft, entry + 8, little),
            0x8769 => exif_ifd_off = read_u32(tiff, entry + 8, little).map(|v| v as usize),
            _ => {}
        }
    }

    // Prefer PixelXDimension/PixelYDimension from ExifIFD — these are the
    // authoritative JPEG pixel counts, excluding MCU alignment padding.
    let mut px_width: Option<u32> = None;
    let mut px_height: Option<u32> = None;
    if let Some(exif_off) = exif_ifd_off
        && exif_off + 2 <= tiff.len()
            && let Some(n) = read_u16(tiff, exif_off, little) {
                for i in 0..n as usize {
                    let entry = exif_off + 2 + i * 12;
                    if entry + 12 > tiff.len() {
                        break;
                    }
                    let tag = read_u16(tiff, entry, little).unwrap_or(0);
                    let ft = read_u16(tiff, entry + 2, little).unwrap_or(0);
                    match tag {
                        0xA002 => px_width = tiff_scalar(tiff, ft, entry + 8, little),
                        0xA003 => px_height = tiff_scalar(tiff, ft, entry + 8, little),
                        _ => {}
                    }
                }
            }

    let source_dims = match (px_width.or(ifd0_width), px_height.or(ifd0_height)) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    };

    // IFD1 holds the embedded thumbnail.
    let ifd1_ptr_off = ifd0_off + 2 + ifd0_count * 12;
    let ifd1_off = read_u32(tiff, ifd1_ptr_off, little)? as usize;
    if ifd1_off == 0 || ifd1_off + 2 > tiff.len() {
        return None;
    }

    let ifd1_count = read_u16(tiff, ifd1_off, little)? as usize;
    let mut jpeg_off: Option<u64> = None;
    let mut jpeg_len: Option<usize> = None;
    for i in 0..ifd1_count {
        let entry = ifd1_off + 2 + i * 12;
        if entry + 12 > tiff.len() {
            break;
        }
        let tag = read_u16(tiff, entry, little)?;
        match tag {
            0x0201 => jpeg_off = read_u32(tiff, entry + 8, little).map(|v| v as u64),
            0x0202 => jpeg_len = read_u32(tiff, entry + 8, little).map(|v| v as usize),
            _ => {}
        }
    }

    let off = jpeg_off?;
    let len = jpeg_len?;
    if len < 4 {
        return None;
    }

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
        4 => read_u32(bytes, offset, little),                   // LONG
        _ => None,
    }
}

//  PNG eXIf embedded thumbnail

/// Scan a file prefix for a PNG `eXIf` chunk and extract its IFD1 thumbnail.
///
/// The PNG `eXIf` chunk (standardised in PNG 1.6) stores raw TIFF data
/// (no `Exif\0\0` prefix) before the first `IDAT` chunk.  For typical camera
/// images this fits entirely within the 4 KiB `HEADER_SCAN` window.
fn find_png_exif_shortcut(bytes: &[u8]) -> Option<JpegExifShortcutInfo> {
    const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 8 || &bytes[0..8] != PNG_SIG {
        return None;
    }
    let mut pos = 8usize;
    while pos + 12 <= bytes.len() {
        let chunk_len =
            u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        let chunk_type = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = data_start.saturating_add(chunk_len).min(bytes.len());
        if chunk_type == b"eXIf" {
            return parse_tiff_exif_thumbnail(&bytes[data_start..data_end], data_start as u64);
        }
        // IDAT/IEND: metadata chunks only appear before image data.
        if chunk_type == b"IDAT" || chunk_type == b"IEND" {
            break;
        }
        // Chunk layout: 4 (length) + 4 (type) + chunk_len (data) + 4 (CRC)
        pos += 8 + chunk_len + 4;
    }
    None
}

/// Read image dimensions from a WebP header.
/// Lossy (VP8):  bytes 23-25 contain a 14-bit width, 14-bit height.
/// Lossless (VP8L): bytes 21-24 contain a 14-bit width+height.
/// Extended (VP8X): bytes 24-27 contain 24-bit width+1, 24-bit height+1.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " if bytes.len() >= 30 => {
            // Lossy: 3-byte frame header at offset 20, then width/height
            let w = u16::from_le_bytes([bytes[26], bytes[27]]) as u32 & 0x3FFF;
            let h = u16::from_le_bytes([bytes[28], bytes[29]]) as u32 & 0x3FFF;
            if w > 0 && h > 0 { Some((w, h)) } else { None }
        }
        b"VP8L" if bytes.len() >= 25 => {
            // Lossless: 5-byte signature at offset 20
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            if w > 1 && h > 1 { Some((w, h)) } else { None }
        }
        b"VP8X" if bytes.len() >= 30 => {
            // Extended: 24-bit width+1, 24-bit height+1
            let w = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
            let h = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
            if w > 1 && h > 1 { Some((w, h)) } else { None }
        }
        _ => None,
    }
}

//  WebP: EXIF chunk (tail-read)

/// WebP stores EXIF data in an `EXIF` chunk that follows the VP8/VP8L image
/// data — it is NOT reachable from the standard 4 KiB header scan.  This
/// function reads the tail of the file, scans for the `EXIF` chunk marker,
/// and delegates to [`parse_tiff_exif_thumbnail`].
async fn try_webp_exif_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let file_size = match cook.http_stream_len() {
        Some(s) if s > 16 => s,
        _ => return,
    };

    // WebP chunks are typically < 1 MB; the EXIF chunk lives near the end.
    let tail_size = (file_size as usize).min(131_072);
    let tail_offset = file_size.saturating_sub(tail_size as u64);
    let tail = match cook.http_read_at(tail_offset, tail_size).await {
        Ok(b) => b,
        Err(_) => return,
    };

    // Scan for the 'EXIF' chunk marker.  The tail starts mid-VP8-data so we
    // can't walk chunk headers — search for the 4-byte marker directly.
    let mut pos = 0usize;
    while pos + 12 <= tail.len() {
        if &tail[pos..pos + 4] == b"EXIF" {
            let chunk_len =
                u32::from_le_bytes([tail[pos + 4], tail[pos + 5], tail[pos + 6], tail[pos + 7]]) as usize;
            let data_start = pos + 8;
            let data_end = (data_start + chunk_len).min(tail.len());
            let file_offset = tail_offset + data_start as u64;
            if let Some(info) = parse_tiff_exif_thumbnail(&tail[data_start..data_end], file_offset) {
                let embedded = match cook.http_read_at(info.thumb_file_offset, info.thumb_len).await {
                    Ok(b) => b,
                    Err(_) => return,
                };
                if embedded.len() < 4 || embedded[0] != 0xFF || embedded[1] != 0xD8 {
                    return;
                }

                let source_dims: Option<(u32, u32)> = if let Some(dims) = info.source_dims {
                    Some(dims)
                } else {
                    // WebP header contains dimensions; parse from the first 30 bytes.
                    cook.http_read_at(0, 4096).await.ok().and_then(|hdr| webp_dimensions(&hdr))
                };
                let Some((prop_w, prop_h)) = source_dims else {
                    return;
                };

                let Ok(img) = image::load_from_memory(&embedded) else {
                    return;
                };
                let color_type = img.color();
                let dl_bytes = cook.http_bytes_fetched();
                let img = pre_scale_to_target(
                    img,
                    ThumbnailConfig::CANONICAL.exact_width,
                    ThumbnailConfig::CANONICAL.exact_height,
                );
                cook.http_close().await;
                cook.render_renderer = Some("shortcut/webp_exif".into());
                cook.render_handler = RenderHandler::Builtin;
                cook.out_download_bytes = dl_bytes;
                if prop_w > 0 && prop_h > 0 {
                    cook.media.properties = Some(image_properties(prop_w, prop_h, color_type));
                }
                cook.render_image = Some(img);
                return;
            }
        }
        pos += 1; // scan byte-by-byte — tail starts mid-VP8-data
    }
}

//  TIFF: embedded JPEG preview

/// Return the `(file_byte_offset, byte_length)` of the best embedded JPEG
/// thumbnail found by traversing the TIFF IFD chain from the file prefix.
fn find_tiff_embedded_jpeg_file_span(bytes: &[u8]) -> Option<(u64, usize)> {
    let (off, len) = find_tiff_embedded_jpeg_span(bytes)?;
    Some((off as u64, len))
}

/// Read the TIFF Orientation tag (0x0112) from IFD0.  Returns 1 (normal) if
/// the tag is absent or the header is too short.
fn tiff_ifd0_orientation(bytes: &[u8], little: bool) -> u16 {
    let ifd0_off = match read_u32(bytes, 4, little) {
        Some(v) => v as usize,
        None => return 1,
    };
    let count = match read_u16(bytes, ifd0_off, little) {
        Some(v) => v as usize,
        None => return 1,
    };
    for i in 0..count {
        let entry = ifd0_off + 2 + i * 12;
        if entry + 12 > bytes.len() {
            break;
        }
        let Some(tag) = read_u16(bytes, entry, little) else {
            break;
        };
        if tag == 0x0112 {
            return read_u16(bytes, entry + 8, little).unwrap_or(1);
        }
    }
    1
}

/// Read IFD0 `ImageWidth` (0x0100) and `ImageLength` (0x0101) from a TIFF
/// header.  Returns `None` when the header is too short or the tags are
/// absent — raw files should fall back to embedded-preview dimensions.
fn tiff_ifd0_dimensions(bytes: &[u8], little: bool) -> Option<(u32, u32)> {
    let ifd0_off = read_u32(bytes, 4, little)? as usize;
    let count = read_u16(bytes, ifd0_off, little)? as usize;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    for i in 0..count {
        let entry = ifd0_off + 2 + i * 12;
        if entry + 12 > bytes.len() {
            break;
        }
        let Some(tag) = read_u16(bytes, entry, little) else {
            break;
        };
        let val = read_u32(bytes, entry + 8, little);
        match tag {
            0x0100 => width = val,
            0x0101 => height = val,
            _ => {}
        }
    }
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    }
}

/// Return `(little_endian, ifd1_file_offset)` by reading IFD0's next-IFD
/// pointer from the TIFF header.
///
/// Used as a fallback when `find_tiff_embedded_jpeg_file_span` returns `None`
/// because IFD1 lies beyond the initial scan window.
fn tiff_endian_and_ifd1_offset(bytes: &[u8]) -> Option<(bool, u64)> {
    if bytes.len() < 8 {
        return None;
    }
    let little = match &bytes[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(bytes, 2, little)? != 42 {
        return None;
    }
    let ifd0_off = read_u32(bytes, 4, little)? as usize;
    if ifd0_off + 2 > bytes.len() {
        return None;
    }
    let count = read_u16(bytes, ifd0_off, little)? as usize;
    let next_ptr_off = ifd0_off.checked_add(2)?.checked_add(count.checked_mul(12)?)?;
    if next_ptr_off + 4 > bytes.len() {
        return None;
    }
    let ifd1_off = read_u32(bytes, next_ptr_off, little)?;
    if ifd1_off == 0 {
        return None;
    }
    Some((little, ifd1_off as u64))
}

fn find_tiff_embedded_jpeg_span(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() < 8 {
        return None;
    }
    let little = match &bytes[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if read_u16(bytes, 2, little)? != 42 {
        return None;
    } // classic TIFF magic

    let ifd0_off = read_u32(bytes, 4, little)? as usize;
    if ifd0_off + 2 > bytes.len() {
        return None;
    }

    let mut queue = VecDeque::from([ifd0_off]);
    let mut visited = HashSet::new();
    let mut best: Option<(usize, usize)> = None;

    while let Some(ifd_off) = queue.pop_front() {
        if !visited.insert(ifd_off) {
            continue;
        }
        let Some((next, subs, joff, jlen)) = parse_tiff_ifd(bytes, ifd_off, little) else {
            continue;
        };

        if next != 0 && next + 2 <= bytes.len() {
            queue.push_back(next);
        }
        for s in subs {
            if s != 0 && s + 2 <= bytes.len() {
                queue.push_back(s);
            }
        }

        if let (Some(o), Some(l)) = (joff, jlen)
            && l >= 4 && best.is_none_or(|(_, bl)| l > bl) {
                best = Some((o, l));
            }
    }
    best
}

#[allow(clippy::type_complexity)]
fn parse_tiff_ifd(
    bytes: &[u8],
    ifd_off: usize,
    little: bool,
) -> Option<(usize, Vec<usize>, Option<usize>, Option<usize>)> {
    if ifd_off + 2 > bytes.len() {
        return None;
    }
    let count = read_u16(bytes, ifd_off, little)? as usize;
    let entries_off = ifd_off + 2;
    let next_ptr_off = entries_off.checked_add(count.checked_mul(12)?)?;
    if next_ptr_off + 4 > bytes.len() {
        return None;
    }

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
        if entry + 12 > bytes.len() {
            break;
        }
        let tag = read_u16(bytes, entry, little)?;
        let ft = read_u16(bytes, entry + 2, little)?;
        let fc = read_u32(bytes, entry + 4, little)? as usize;
        let v = read_u32(bytes, entry + 8, little)? as usize;

        match tag {
            0x0103 => compression = Some(v as u16),
            0x0201 => jpeg_off = Some(v),
            0x0202 => jpeg_len = Some(v),
            0x0111 => {
                strip_off = tiff_u32_values(bytes, ft, fc, v, little, 1)
                    .first()
                    .copied()
                    .map(|x| x as usize)
            }
            0x0117 => {
                strip_cnt = tiff_u32_values(bytes, ft, fc, v, little, 1)
                    .first()
                    .copied()
                    .map(|x| x as usize)
            }
            0x0144 => {
                tile_off = tiff_u32_values(bytes, ft, fc, v, little, 1)
                    .first()
                    .copied()
                    .map(|x| x as usize)
            }
            0x0145 => {
                tile_cnt = tiff_u32_values(bytes, ft, fc, v, little, 1)
                    .first()
                    .copied()
                    .map(|x| x as usize)
            }
            0x014A => sub_ifds.extend(tiff_subifd_offsets(bytes, ft, fc, v, little)),
            _ => {}
        }
    }

    // JPEG-compressed IFDs may use strip/tile layout instead of JPEGInterchangeFormat.
    if (jpeg_off.is_none() || jpeg_len.is_none())
        && matches!(compression, Some(6 | 7)) {
            if let (Some(o), Some(l)) = (strip_off, strip_cnt) {
                jpeg_off = Some(o);
                jpeg_len = Some(l);
            } else if let (Some(o), Some(l)) = (tile_off, tile_cnt) {
                jpeg_off = Some(o);
                jpeg_len = Some(l);
            }
        }

    let next_ifd = read_u32(bytes, next_ptr_off, little)? as usize;
    Some((next_ifd, sub_ifds, jpeg_off, jpeg_len))
}

fn tiff_subifd_offsets(bytes: &[u8], ft: u16, fc: usize, v: usize, little: bool) -> Vec<usize> {
    if fc == 0 || ft != 4 {
        return Vec::new();
    }
    if fc == 1 {
        return vec![v];
    }
    (0..fc.min(32))
        .filter_map(|i| read_u32(bytes, v + i * 4, little).map(|x| x as usize))
        .collect()
}

fn tiff_u32_values(bytes: &[u8], ft: u16, fc: usize, v: usize, little: bool, max: usize) -> Vec<u32> {
    let count = fc.min(max);
    if count == 0 {
        return Vec::new();
    }
    match ft {
        3 => {
            // SHORT
            if fc == 1 {
                vec![v as u32]
            } else {
                (0..count)
                    .filter_map(|i| read_u16(bytes, v + i * 2, little).map(|x| x as u32))
                    .collect()
            }
        }
        4 => {
            // LONG
            if fc == 1 {
                vec![v as u32]
            } else {
                (0..count).filter_map(|i| read_u32(bytes, v + i * 4, little)).collect()
            }
        }
        _ => Vec::new(),
    }
}

//  Power-of-two pre-scale 

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
        if w / next >= tw * 2 && h / next >= th * 2 {
            divisor = next;
        } else {
            break;
        }
    }
    let img = if divisor > 1 {
        let steps = divisor.trailing_zeros();
        let mut img = img;
        for _ in 0..steps {
            img = box_half(img);
        }
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
    if ow == 0 || oh == 0 {
        return img;
    }

    // Luma8 — 1 byte/pixel
    if matches!(img, DynamicImage::ImageLuma8(_)) {
        let mut buf = img.into_luma8().into_raw();
        let stride = w as usize;
        for oy in 0..oh as usize {
            let row0 = oy * 2 * stride;
            let row1 = row0 + stride;
            let dst = oy * ow as usize;
            for ox in 0..ow as usize {
                let sx = ox * 2;
                buf[dst + ox] = ((buf[row0 + sx] as u16
                    + buf[row0 + sx + 1] as u16
                    + buf[row1 + sx] as u16
                    + buf[row1 + sx + 1] as u16
                    + 2)
                    >> 2) as u8;
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
            let dst = oy * ow as usize * 4;
            for ox in 0..ow as usize {
                let sx = ox * 8; // ox * 2 pixels * 4 channels
                for c in 0..4usize {
                    buf[dst + ox * 4 + c] = ((buf[row0 + sx + c] as u16
                        + buf[row0 + sx + 4 + c] as u16
                        + buf[row1 + sx + c] as u16
                        + buf[row1 + sx + 4 + c] as u16
                        + 2)
                        >> 2) as u8;
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
        let dst = oy * ow as usize * 3;
        for ox in 0..ow as usize {
            let sx = ox * 6; // ox * 2 pixels * 3 channels
            for c in 0..3usize {
                buf[dst + ox * 3 + c] = ((buf[row0 + sx + c] as u16
                    + buf[row0 + sx + 3 + c] as u16
                    + buf[row1 + sx + c] as u16
                    + buf[row1 + sx + 3 + c] as u16
                    + 2)
                    >> 2) as u8;
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
    let per_channel = color_type.bits_per_pixel() as u32 / color_type.channel_count() as u32;
    let color_channels = if color_type.has_alpha() {
        color_type.channel_count() as u32 - 1
    } else {
        color_type.channel_count() as u32
    };
    let bits_per_pixel = per_channel * color_channels;
    serde_json::json!({
        "width":  src_w,
        "height": src_h,
        "bpp": bits_per_pixel,
        "alpha": color_type.has_alpha() as i32,
        "lossless": 0,
    })
}

//  Byte readers

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
    Some(if little {
        u32::from_le_bytes([b0, b1, b2, b3])
    } else {
        u32::from_be_bytes([b0, b1, b2, b3])
    })
}

//  Audio: ID3v2 APIC cover art shortcut 

/// Attempt to extract embedded cover art from an audio file's ID3v2 tag.
///
/// Supports MP3 files with ID3v2.x APIC (Attached Picture) frames.
///
/// Strategy:
/// - Read the ID3v2 header from the page cache (zero extra network I/O).
/// - Read up to `audio_cover_max_fetch` bytes total starting at byte 0.
///   Since ID3v2 tags are always at the beginning of the file, this is
///   guaranteed to include the full tag for any reasonably-sized cover art.
///   We read based on the file size (up to the limit) rather than trusting
///   the ID3 header's `tag_size` field, which some encoders under-report.
/// - Scan frames for "APIC"; extract and decode the embedded image.
async fn try_audio_shortcut<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let config = &ThumbnailConfig::CANONICAL;

    // Read enough bytes to cover metadata.  Use the file size (up to the
    // configured limit) rather than trusting the ID3 tag size.
    let fetch_limit = cook.runtime.shortcut_limits.audio_cover_max_fetch as u64;
    let file_size = cook.http_stream_len().unwrap_or(u64::MAX);
    let read_len = file_size.min(fetch_limit) as usize;

    let tag_bytes = match cook.http_read_at(0, read_len).await {
        Ok(b) => b,
        Err(_) => return,
    };

    //  Detect ID3v2 header (optional) 
    let id3 = parse_id3_header(&tag_bytes);

    //  Locate the first MPEG audio frame
    // With ID3: starts 10 + tag_size bytes in.  Without: starts at byte 0.
    let audio_body_offset = id3.as_ref().map(|h| 10usize + h.tag_size).unwrap_or(0);

    let (channel_count, duration_secs) = if tag_bytes.len() > audio_body_offset + 4 {
        parse_first_mp3_frame(&tag_bytes[audio_body_offset..])
            .map(|frame| {
                let chan = frame.channel_count() as u32;
                let dur = if frame.bitrate > 0 {
                    let audio_bytes = file_size.saturating_sub(audio_body_offset as u64);
                    Some((audio_bytes * 8) as f64 / frame.bitrate as f64)
                } else {
                    None
                };
                (chan, dur)
            })
            .unwrap_or((0, None))
    } else {
        (0, None)
    };

    let mut props = serde_json::json!({});
    if channel_count > 0 {
        props["channel_count"] = serde_json::json!(channel_count);
    }
    if let Some(d) = duration_secs {
        props["duration_seconds"] = serde_json::json!(d);
    }

    //  Scan ID3v2 frames for APIC cover art 
    let image_bytes = id3.and_then(|h| find_id3_apic(&tag_bytes, h.version_major));

    let Some(image_bytes) = image_bytes else {
        // No cover art, but we still have audio metadata.
        if channel_count > 0 || duration_secs.is_some() {
            cook.media.properties = Some(props);
        }
        return;
    };

    let dl_bytes = cook.http_bytes_fetched();
    cook.http_close().await;

    let Ok(img) = image::load_from_memory(&image_bytes) else {
        return;
    };
    let img = pre_scale_to_target(img, config.exact_width, config.exact_height);

    cook.media.properties = Some(props);

    cook.render_renderer = Some("shortcut/audio".into());
    cook.render_handler = RenderHandler::Builtin;
    cook.out_download_bytes = dl_bytes;
    cook.render_image = Some(img);
}

/// Parsed ID3v2 header fields.
struct Id3Header {
    version_major: u8,
    /// Total size of the tag body (frames + padding), NOT including the
    /// 10-byte header.  Always a 28-bit synchsafe integer.
    #[allow(dead_code)]
    tag_size: usize,
    /// Whether an extended header is present (flag bit 6).
    has_extended_header: bool,
    /// Whether a footer is present (flag bit 4, v2.4 only).
    #[allow(dead_code)]
    has_footer: bool,
}

/// Parse the ID3v2 header from the first bytes of a file.
/// Returns `None` if the magic `"ID3"` is absent or the header is truncated.
fn parse_id3_header(bytes: &[u8]) -> Option<Id3Header> {
    if bytes.len() < 10 {
        return None;
    }
    if &bytes[0..3] != b"ID3" {
        return None;
    }

    let version_major = bytes[3];
    let _version_minor = bytes[4];
    let flags = bytes[5];

    // ID3v2.2 is ancient; we only support ≥ v2.3.
    if version_major < 3 {
        return None;
    }

    let has_extended_header = (flags & 0x40) != 0;
    let has_footer = version_major >= 4 && (flags & 0x10) != 0;

    // All four size bytes are 28-bit synchsafe (MSB of each byte is 0).
    let tag_size = synchsafe_u32(&bytes[6..10]) as usize;

    Some(Id3Header {
        version_major,
        tag_size,
        has_extended_header,
        has_footer,
    })
}

/// Decode a 4-byte synchsafe integer (each byte uses only 7 bits, MSB is 0).
fn synchsafe_u32(bytes: &[u8]) -> u32 {
    let b0 = bytes.first().copied().unwrap_or(0) & 0x7F;
    let b1 = bytes.get(1).copied().unwrap_or(0) & 0x7F;
    let b2 = bytes.get(2).copied().unwrap_or(0) & 0x7F;
    let b3 = bytes.get(3).copied().unwrap_or(0) & 0x7F;
    (b0 as u32) << 21 | (b1 as u32) << 14 | (b2 as u32) << 7 | b3 as u32
}

/// Decode a regular big-endian u32 from 4 bytes.
fn be_u32(bytes: &[u8]) -> u32 {
    let b0 = bytes.first().copied().unwrap_or(0) as u32;
    let b1 = bytes.get(1).copied().unwrap_or(0) as u32;
    let b2 = bytes.get(2).copied().unwrap_or(0) as u32;
    let b3 = bytes.get(3).copied().unwrap_or(0) as u32;
    b0 << 24 | b1 << 16 | b2 << 8 | b3
}

/// Scan an ID3v2 tag body (the bytes after the 10-byte header) for an APIC
/// frame and return the embedded image bytes.
///
/// `bytes` starts at byte 0 of the file (including the ID3 header).
/// `version_major` is 3 or 4 — determines whether frame sizes are synchsafe.
///
/// Scanning stops at the declared tag end OR when a non-ASCII-uppercase
/// frame ID is encountered (padding or audio data), whichever comes first.
/// This is robust against encoders that under-report the tag size.
///
/// # ID3v2 frame header layout (10 bytes)
///
/// | Offset | Size | Field |
/// |--------|------|-------|
/// | 0      | 4    | Frame ID (e.g. `"APIC"`) |
/// | 4      | 4    | Size (synchsafe for v2.3, big-endian for v2.4) |
/// | 8      | 2    | Flags |
fn find_id3_apic(bytes: &[u8], version_major: u8) -> Option<Vec<u8>> {
    let id3 = parse_id3_header(bytes)?;

    // Start of the tag body (after the 10-byte header).
    let mut pos: usize = 10;

    // Skip extended header if present.
    if id3.has_extended_header {
        if pos + 4 > bytes.len() {
            return None;
        }
        let ext_size = synchsafe_u32(&bytes[pos..pos + 4]) as usize;
        // Extended header size includes the 4-byte size field itself.
        pos = pos.checked_add(ext_size)?;
    }

    // Scan frames until we hit a non-valid frame ID (padding 0x00 or
    // audio data 0xFF…) or run out of buffer.  We don't use id3.tag_size
    // as a hard limit because some encoders under-report it.
    while pos + 10 <= bytes.len() {
        let frame_id = &bytes[pos..pos + 4];

        // Valid ID3v2 frame IDs: [A-Z][A-Z0-9][A-Z0-9][A-Z0-9]
        if !is_valid_frame_id(frame_id) {
            break;
        }

        // Frame size: big-endian for v2.3, synchsafe for v2.4.
        let frame_size: usize = if version_major >= 4 {
            synchsafe_u32(&bytes[pos + 4..pos + 8]) as usize
        } else {
            be_u32(&bytes[pos + 4..pos + 8]) as usize
        };

        // Frame data starts after the 10-byte frame header.
        let data_start = pos + 10;

        if frame_id == b"APIC" {
            let data_end = data_start.checked_add(frame_size)?.min(bytes.len());
            let apic_data = &bytes[data_start..data_end];
            return extract_apic_image(apic_data);
        }

        // Advance to next frame.
        pos = data_start.checked_add(frame_size)?;

        // If we've passed the declared end of the tag and haven't found
        // APIC yet, continue scanning anyway — the tag size may have been
        // underreported.  But don't go past the end of our buffer.
    }

    None
}

/// Check whether a 4-byte slice is a valid ID3v2 frame identifier.
///
/// Valid frame IDs are `[A-Z][A-Z0-9][A-Z0-9][A-Z0-9]`.
fn is_valid_frame_id(id: &[u8]) -> bool {
    id.len() == 4
        && id[0].is_ascii_uppercase()
        && id[1].is_ascii_alphanumeric()
        && id[1].is_ascii_uppercase()
        && id[2].is_ascii_alphanumeric()
        && id[2].is_ascii_uppercase()
        && id[3].is_ascii_alphanumeric()
}

/// Extract the raw image bytes from an APIC frame data payload.
///
/// APIC frame layout (ID3v2.3 §4.15 / v2.4 §4.14):
///
/// | Offset | Size   | Field |
/// |--------|--------|-------|
/// | 0      | 1      | Text encoding (0=ISO-8859-1, 3=UTF-8) |
/// | 1      | var    | MIME type (null-terminated) |
/// | …      | 1      | Picture type (3 = Cover (front)) |
/// | …      | var    | Description (null-terminated) |
/// | …      | rest   | Binary image data |
fn extract_apic_image(apic_data: &[u8]) -> Option<Vec<u8>> {
    if apic_data.len() < 4 {
        return None;
    }

    let encoding = apic_data[0];

    // Find the null terminator for the MIME type.
    // For encoding 0 (ISO-8859-1) and 3 (UTF-8), null is a single 0x00 byte.
    // For encoding 1/2 (UTF-16), null is two 0x00 bytes.
    let mime_start = 1usize;
    let mime_end = match encoding {
        1 | 2 => {
            // UTF-16: find double-null (0x00 0x00) at an even offset.
            let mut i = mime_start;
            while i + 1 < apic_data.len() {
                if apic_data[i] == 0x00 && apic_data[i + 1] == 0x00 {
                    break;
                }
                i += 2;
            }
            if i + 1 >= apic_data.len() {
                return None;
            }
            i + 2 // skip past the double null
        }
        _ => {
            // ISO-8859-1 or UTF-8: single null byte.
            let end = apic_data[mime_start..].iter().position(|&b| b == 0x00)?;
            mime_start + end + 1 // skip past the null
        }
    };

    if mime_end >= apic_data.len() {
        return None;
    }

    // Skip picture type byte.
    let desc_start = mime_end + 1;
    if desc_start >= apic_data.len() {
        return None;
    }

    // Find the null terminator for the description.
    let image_start = match encoding {
        1 | 2 => {
            let mut i = desc_start;
            while i + 1 < apic_data.len() {
                if apic_data[i] == 0x00 && apic_data[i + 1] == 0x00 {
                    break;
                }
                i += 2;
            }
            if i + 1 >= apic_data.len() {
                return None;
            }
            i + 2
        }
        _ => {
            let end = apic_data[desc_start..].iter().position(|&b| b == 0x00)?;
            desc_start + end + 1
        }
    };

    if image_start >= apic_data.len() {
        return None;
    }

    Some(apic_data[image_start..].to_vec())
}

//  Public pipeline step (merged) 

// SMALL_FILE_THRESHOLD is now runtime-configurable via
// cook.runtime.shortcut_limits.small_file_threshold.
// See spec::ShortcutLimits for tier-specific values.

/// Try to produce a thumbnail without a full upstream decode.
///
/// Six active paths, in priority order:
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

    //  EXIF embedded thumbnail (JPEG EXIF IFD1 / TIFF IFD chain) 
    //
    // Attempt this BEFORE the small-image full-download path so that JPEG/TIFF
    // files with an embedded thumbnail are served from the thumbnail bytes only
    // (typically a few hundred bytes header + a few KB for the thumbnail) rather
    // than downloading the entire source file.
    let is_jpeg = matches!(ext, "jpeg");
    let is_tiff = matches!(ext, "tiff");
    if is_jpeg || is_tiff || ext == "png" {
        try_exif_shortcut(cook).await;
        if !cook.http_is_open() {
            return;
        }
    }
    if ext == "webp" {
        try_webp_exif_shortcut(cook).await;
        if !cook.http_is_open() {
            return;
        }
    }

    //  Small image: known `image`-crate formats, ≤ SMALL_FILE_THRESHOLD 
    //
    // `inspect` has already classified the file; trust its verdict.
    //
    // Use an explicit positive list so formats that tier1's image build does
    // NOT support (HEIC, AVIF, EXR, …) never reach load_from_memory.  This
    // also protects against future additions to FileKind::Image that tier2
    // should handle via libav.
    //
    // Keep this BEFORE the progressive JPEG path so genuinely small files
    // always use a full decode for reliability/quality.
    let is_tier1_image_format = matches!(ext, "jpeg" | "png" | "gif" | "bmp" | "tiff" | "ico");
    let is_small_image = cook.media.kind == Some(FileKind::Image)
        && is_tier1_image_format
        && cook
            .http_stream_len()
            .map(|n| n <= cook.runtime.shortcut_limits.small_file_threshold)
            .unwrap_or(false);

    if is_small_image {
        let file_size = cook.http_stream_len().unwrap_or(0);

        // read_at has pread semantics: the cursor is saved and restored, and
        // streaming mode is never entered.  If load_from_memory fails the
        // buffer is left intact at cursor 0 so tier2 can call take_reader().
        let data = cook.http_read_at(0, file_size as usize).await.unwrap_or_default();

        if !data.is_empty()
            && let Ok(img) = image::load_from_memory(&data) {
                let (src_w, src_h) = (img.width(), img.height());
                let color_type = img.color();
                let dl_bytes = cook.http_bytes_fetched();

                cook.http_close().await;

                let img = pre_scale_to_target(img, config.exact_width, config.exact_height);

                cook.render_renderer = Some("shortcut/small".into());
                cook.render_handler = RenderHandler::Builtin;
                cook.out_download_bytes = dl_bytes;
                if src_w > 0 && src_h > 0 {
                    cook.media.properties = Some(image_properties(src_w, src_h, color_type));
                }
                cook.render_image = Some(img);
                return;
            }
            // load_from_memory failed — buffer is still intact (read_at
            // restored the cursor; streaming mode was never entered).
            // Fall through so tier2 can handle the format via libav.
    }

    //  Progressive JPEG (falls through from EXIF and small-image checks)
    if is_jpeg {
        try_progressive_jpeg_shortcut(cook).await;
        if !cook.http_is_open() {
            return;
        }
    }

    //  Camera-raw embedded JPEG preview
    let is_raw = cook.media.kind == Some(FileKind::Image)
        && matches!(
            cook.media.extension.as_deref(),
            Some("dng" | "cr2" | "nef" | "arw" | "orf" | "rw2" | "pef" | "srw" | "3fr" | "mef" | "rwl")
        );
    if is_raw {
        try_raw_shortcut(cook).await;
        if !cook.http_is_open() {
            return;
        }
    }

    //  ZIP containers (ODT, DOCX, …) 
    let is_zip_doc = cook.media.kind == Some(FileKind::Document)
        && matches!(
            cook.media.extension.as_deref(),
            Some("docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp")
        );
    if is_zip_doc {
        try_zip_shortcut(cook).await;
    }

    //  Audio: ID3v2 APIC cover art
    let is_audio =
        cook.media.kind == Some(FileKind::Audio) && matches!(cook.media.extension.as_deref(), Some("mp3"));
    if is_audio {
        try_audio_shortcut(cook).await;
    }
}

//  MP3 frame header parser 

/// Parsed first MPEG audio frame header (4 bytes after sync).
#[derive(Debug)]
struct Mp3FrameInfo {
    /// Bitrate in bits per second.
    bitrate: u32,
    /// Sample rate in Hz.
    #[allow(dead_code)]
    sample_rate: u32,
    /// Channel mode: 0=stereo, 1=joint, 2=dual, 3=mono.
    channel_mode: u8,
}

impl Mp3FrameInfo {
    fn channel_count(&self) -> u8 {
        match self.channel_mode {
            3 => 1, // mono
            _ => 2, // stereo / joint / dual
        }
    }
}

/// Attempt to parse the first MPEG audio frame from `bytes`, which should
/// start at the first byte after the ID3v2 tag (i.e. at the sync word).
///
/// Returns `None` if no valid MPEG frame header is found within the first
/// 2 KiB (should be immediate for standard MP3 files).
fn parse_first_mp3_frame(bytes: &[u8]) -> Option<Mp3FrameInfo> {
    // Search for a sync word within the first 2 KiB.
    let limit = bytes.len().min(2048);
    for i in 0..limit.saturating_sub(1) {
        if bytes[i] == 0xFF && (bytes[i + 1] & 0xE0) == 0xE0
            && let Some(info) = parse_mp3_frame_header(&bytes[i..]) {
                return Some(info);
            }
    }
    None
}

/// Parse a single MPEG audio frame header starting at `bytes[0]`.
fn parse_mp3_frame_header(bytes: &[u8]) -> Option<Mp3FrameInfo> {
    if bytes.len() < 4 {
        return None;
    }

    let hdr = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

    // Sync: 11 bits of 1.
    if (hdr >> 21) != 0x7FF {
        return None;
    }

    let version = (hdr >> 19) & 0x3; // 00=MPEG2.5, 10=MPEG2, 11=MPEG1
    let layer = (hdr >> 17) & 0x3; // 01=Layer3, 10=Layer2, 11=Layer1
    let bitrate_idx = ((hdr >> 12) & 0xF) as usize;
    let sample_rate_idx = ((hdr >> 10) & 0x3) as usize;
    let channel_mode = ((hdr >> 6) & 0x3) as u8;

    // Only Layer III is common for MP3 files.
    if layer != 0x01 {
        return None;
    }

    let bitrate = match version {
        3 => MPEG1_LAYER3_BITRATE.get(bitrate_idx).copied()?, // MPEG1
        2 => MPEG2_LAYER3_BITRATE.get(bitrate_idx).copied()?, // MPEG2
        _ => MPEG25_LAYER3_BITRATE.get(bitrate_idx).copied()?, // MPEG2.5
    };
    let sample_rate = match version {
        3 => MPEG1_SAMPLE_RATE.get(sample_rate_idx).copied()?,
        2 => MPEG2_SAMPLE_RATE.get(sample_rate_idx).copied()?,
        _ => MPEG25_SAMPLE_RATE.get(sample_rate_idx).copied()?,
    };

    if bitrate == 0 {
        return None;
    } // "free" bitrate — can't estimate

    Some(Mp3FrameInfo {
        bitrate: bitrate * 1000,
        sample_rate,
        channel_mode,
    })
}

// Bitrate tables in kbps, index 0 = free (invalid for estimation).

const MPEG1_LAYER3_BITRATE: [u32; 16] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];

const MPEG2_LAYER3_BITRATE: [u32; 16] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0];

const MPEG25_LAYER3_BITRATE: [u32; 16] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0];

const MPEG1_SAMPLE_RATE: [u32; 4] = [44100, 48000, 32000, 0];
const MPEG2_SAMPLE_RATE: [u32; 4] = [22050, 24000, 16000, 0];
const MPEG25_SAMPLE_RATE: [u32; 4] = [11025, 12000, 8000, 0];
