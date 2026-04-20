//! Core synchronous processing pipeline for common still-image inputs.
//!
//! Design rule: every input image goes through this same decode -> process ->
//! encode path. Even if a source contains an embedded thumbnail, we still run
//! it through post-processing so no output pixel bypasses the pipeline.

use image::imageops::{FilterType, crop_imm, resize};
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba, RgbaImage, metadata::Orientation};
use mozjpeg::{ColorSpace, Compress, Marker};
use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::io::Cursor;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use crate::{app_config, DeveloperData, ItemRequest, ItemResult, JobStatus, MediaLogData, SourceMetadata, SourceRef, ThumbnailProfile, http_source};
use crate::http_source::ConditionalRequest;
use crate::ThumbnailRequestState;
use zip::read::ZipArchive;

const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
// Prefix read for remote sources should be large enough to cover common
// progressive partial decode windows without requiring a second fetch.
const REMOTE_PREFIX_READ_BYTES: usize = 256 * 1024;

static TRANSPARENCY_BACKGROUND: OnceLock<Option<RgbaImage>> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
enum DecodeStrategy {
    FullImage,
    EmbeddedJpegThumbnail,
    ProgressivePartial,
    PngInterlacedPartial,
    OdtPackageThumbnail,
    DocxPackageThumbnail,
}

impl DecodeStrategy {
    fn as_str(self) -> &'static str {
        match self {
            DecodeStrategy::FullImage => "full_image",
            DecodeStrategy::EmbeddedJpegThumbnail => "embedded_jpeg_thumbnail",
            DecodeStrategy::ProgressivePartial => "progressive_partial",
            DecodeStrategy::PngInterlacedPartial => "png_interlaced_partial",
            DecodeStrategy::OdtPackageThumbnail => "odt_package_thumbnail",
            DecodeStrategy::DocxPackageThumbnail => "docx_package_thumbnail",
        }
    }

}

const TIER2_EMBEDDED_HEIC_THUMBNAIL_STRATEGY: &str = "tier2_embedded_heic_thumbnail";

fn is_embedded_thumbnail_strategy(decode_strategy: &str) -> bool {
    decode_strategy == DecodeStrategy::EmbeddedJpegThumbnail.as_str()
        || decode_strategy == TIER2_EMBEDDED_HEIC_THUMBNAIL_STRATEGY
}

/// Render summary produced by the image post-process pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct RenderInfo {
    pub stream_bytes_read: u64,
    pub decode_strategy: String,
    pub input_width: u32,
    pub input_height: u32,
    pub decoded_width: u32,
    pub decoded_height: u32,
    pub source_orientation: String,
    pub pixel_art_mode: bool,
    pub output_width: u32,
    pub output_height: u32,
    pub output_quality: u8,
}

/// Build source metadata for a local byte source.
pub fn metadata_from_local_bytes(bytes: &[u8], content_length: Option<u64>, last_modified: Option<String>) -> SourceMetadata {
    let magic_mime = infer::get(bytes).map(|k| k.mime_type().to_string());
    let file_kind = crate::media::sniff_file_kind(bytes, None);
    SourceMetadata {
        content_type: None,
        magic_mime,
        content_length,
        etag: None,
        last_modified,
        accepts_ranges: false,
        file_kind,
        // Local files have no URL redirect chain or remote cache key.
        canonical_url: None,
        cache_key: None,
    }
}

/// Decode bytes, run through the canonical post-process pipeline, and return
/// a low-quality JPEG plus render details.
pub fn render_thumbnail_from_bytes(bytes: &[u8], profile: &ThumbnailProfile) -> Result<(Vec<u8>, RenderInfo), String> {
    render_thumbnail_from_bytes_with_stream_bytes(bytes, profile, bytes.len() as u64)
}

/// Decode bytes, run through the canonical post-process pipeline, and return
/// a low-quality JPEG plus render details. `stream_bytes_read` reflects how
/// many bytes were consumed from upstream transport for this render attempt.
pub fn render_thumbnail_from_bytes_with_stream_bytes(
    bytes: &[u8],
    profile: &ThumbnailProfile,
    stream_bytes_read: u64,
) -> Result<(Vec<u8>, RenderInfo), String> {
    // Choose the cheapest viable decode source for the render pipeline.
    let (img, strategy) = decode_image_with_strategy(bytes)?;

    // Source-level metadata should come from the full image stream when that
    // stream is itself an image. For container-derived thumbnails (ODT/DOCX),
    // fall back to decoded image dimensions and no orientation transform.
    let source_probe = probe_source_image_info(bytes);
    let orientation = source_probe
        .as_ref()
        .map(|(_, _, o)| *o)
        .unwrap_or(Orientation::NoTransforms);

    let decoded_width = img.width();
    let decoded_height = img.height();
    let (source_width, source_height) = source_probe
        .map(|(w, h, _)| (w, h))
        .unwrap_or((decoded_width, decoded_height));

    render_thumbnail_from_dynamic_image_with_options(
        img,
        profile,
        stream_bytes_read,
        strategy.as_str(),
        source_width,
        source_height,
        orientation,
    )
}

/// Render from an already-decoded image using the canonical thumbnail process.
///
/// This is used by Tier 2 loaders (e.g. libav decode) so they share the same
/// post-process and JPEG encode behavior as Tier 1 byte decoders.
pub fn render_thumbnail_from_dynamic_image_with_stream_bytes(
    img: DynamicImage,
    profile: &ThumbnailProfile,
    stream_bytes_read: u64,
    decode_strategy: &str,
) -> Result<(Vec<u8>, RenderInfo), String> {
    let source_width = img.width();
    let source_height = img.height();
    render_thumbnail_from_dynamic_image_with_source_dimensions(
        img,
        profile,
        stream_bytes_read,
        decode_strategy,
        source_width,
        source_height,
    )
}

/// Render from an already-decoded image while preserving source dimensions
/// from the upstream decoder for metadata reporting.
pub fn render_thumbnail_from_dynamic_image_with_source_dimensions(
    img: DynamicImage,
    profile: &ThumbnailProfile,
    stream_bytes_read: u64,
    decode_strategy: &str,
    source_width: u32,
    source_height: u32,
) -> Result<(Vec<u8>, RenderInfo), String> {
    render_thumbnail_from_dynamic_image_with_options(
        img,
        profile,
        stream_bytes_read,
        decode_strategy,
        source_width,
        source_height,
        Orientation::NoTransforms,
    )
}

fn render_thumbnail_from_dynamic_image_with_options(
    mut img: DynamicImage,
    profile: &ThumbnailProfile,
    stream_bytes_read: u64,
    decode_strategy: &str,
    source_width: u32,
    source_height: u32,
    orientation: Orientation,
) -> Result<(Vec<u8>, RenderInfo), String> {
    // Let the image crate apply EXIF orientation transforms so camera images
    // land upright without custom orientation parsing logic.
    img.apply_orientation(orientation);

    let decoded_width = img.width();
    let decoded_height = img.height();

    // Embedded JPEG thumbnails are photographic content — use Lanczos3 when
    // upscaling them, not Nearest (which is only appropriate for pixel art).
    let is_embedded = is_embedded_thumbnail_strategy(decode_strategy);

    let mut buf = ProcessBuffer::new(img);
    let scaled_up = buf.fill_crop(profile.width, profile.height, is_embedded);
    buf.composite_transparency_over_background();

    // Embedded EXIF thumbnails should not be pushed into pixel-art mode.
    let pixel_art_mode = scaled_up && !is_embedded;
    buf.apply_color_pipeline(pixel_art_mode, profile.vignette_strength);

    // Pixel-art mode uses lower JPEG quality to keep output size bounded.
    let effective_quality = if pixel_art_mode {
        profile.pixel_art_quality
    } else {
        profile.quality
    };

    let thumb = buf.encode_jpeg(effective_quality)?;
    let info = RenderInfo {
        stream_bytes_read,
        decode_strategy: decode_strategy.to_string(),
        input_width: source_width,
        input_height: source_height,
        decoded_width,
        decoded_height,
        source_orientation: orientation_name(orientation).to_string(),
        pixel_art_mode,
        output_width: profile.width,
        output_height: profile.height,
        output_quality: effective_quality,
    };

    Ok((thumb, info))
}

fn probe_source_image_info(bytes: &[u8]) -> Option<(u32, u32, Orientation)> {
    let cursor = Cursor::new(bytes);
    let reader = ImageReader::new(cursor).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().ok()?;
    Some((w, h, orientation))
}

fn decode_image_with_strategy(bytes: &[u8]) -> Result<(DynamicImage, DecodeStrategy), String> {
    if let Some((img, strategy)) = try_decode_container_thumbnail(bytes) {
        return Ok((img, strategy));
    }

    if let Some(thumb) = extract_tiff_embedded_jpeg_thumbnail(bytes) {
        if let Ok(img) = image::load_from_memory(&thumb) {
            return Ok((img, DecodeStrategy::EmbeddedJpegThumbnail));
        }
    }

    if tiff_is_probably_raw_sensor(bytes) {
        return Err("raw image without embedded JPEG preview is unsupported".to_string());
    }

    if let Some(png) = inspect_png(bytes) {
        let partial_read_bytes = png_partial_read_bytes(&png, bytes.len());
        if png.is_interlaced && bytes.len() > partial_read_bytes {
            let partial = &bytes[..partial_read_bytes];
            if let Ok(img) = decode_partial_interlaced_png(partial) {
                return Ok((img, DecodeStrategy::PngInterlacedPartial));
            }
        }
    }

    if let Some(jpeg) = inspect_jpeg(bytes) {
        if let Some(thumb) = jpeg.embedded_thumbnail_jpeg.as_deref() {
            if let Ok(img) = image::load_from_memory(thumb) {
                return Ok((img, DecodeStrategy::EmbeddedJpegThumbnail));
            }
        }

        let partial_read_bytes = progressive_partial_read_bytes(&jpeg, bytes.len());
        if jpeg.is_progressive && bytes.len() > partial_read_bytes {
            let partial = &bytes[..partial_read_bytes];
            if let Ok(img) = decode_partial_progressive(partial) {
                return Ok((img, DecodeStrategy::ProgressivePartial));
            }
        }
    }

    let img = image::load_from_memory(bytes)
        .map_err(|err| format!("unsupported or invalid image: {err}"))?;
    Ok((img, DecodeStrategy::FullImage))
}

fn try_decode_container_thumbnail(bytes: &[u8]) -> Option<(DynamicImage, DecodeStrategy)> {
    if bytes.len() < 4 || &bytes[0..4] != b"PK\x03\x04" {
        return None;
    }

    let cursor = Cursor::new(bytes);
    let mut zip = ZipArchive::new(cursor).ok()?;

    // ODT strict policy: only accept the standard package thumbnail path.
    if zip_is_odt(&mut zip) {
        if let Some(img) = decode_zip_entry_image(&mut zip, "Thumbnails/thumbnail.png") {
            return Some((img, DecodeStrategy::OdtPackageThumbnail));
        }
        return None;
    }

    // DOCX strict policy: only accept the expected package thumbnail path.
    if zip_is_docx(&mut zip) {
        if let Some(img) = decode_zip_entry_image(&mut zip, "docProps/thumbnail.jpeg") {
            return Some((img, DecodeStrategy::DocxPackageThumbnail));
        }
        if let Some(img) = decode_zip_entry_image(&mut zip, "docProps/thumbnail.jpg") {
            return Some((img, DecodeStrategy::DocxPackageThumbnail));
        }
        if let Some(img) = decode_zip_entry_image(&mut zip, "docProps/thumbnail.png") {
            return Some((img, DecodeStrategy::DocxPackageThumbnail));
        }
    }

    None
}

fn zip_is_odt(zip: &mut ZipArchive<Cursor<&[u8]>>) -> bool {
    let Ok(mut f) = zip.by_name("mimetype") else {
        return false;
    };

    let mut buf = vec![0u8; 256];
    let Ok(n) = std::io::Read::read(&mut f, &mut buf) else {
        return false;
    };

    let s = String::from_utf8_lossy(&buf[..n]);
    s.trim() == "application/vnd.oasis.opendocument.text"
}

fn zip_is_docx(zip: &mut ZipArchive<Cursor<&[u8]>>) -> bool {
    let Ok(mut f) = zip.by_name("[Content_Types].xml") else {
        return false;
    };

    let mut xml = String::new();
    if std::io::Read::read_to_string(&mut f, &mut xml).is_err() {
        return false;
    }

    xml.contains("application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml")
}

fn decode_zip_entry_image(zip: &mut ZipArchive<Cursor<&[u8]>>, path: &str) -> Option<DynamicImage> {
    let mut f = zip.by_name(path).ok()?;
    let mut data = Vec::new();
    if std::io::Read::read_to_end(&mut f, &mut data).is_err() {
        return None;
    }

    image::load_from_memory(&data).ok()
}

fn decode_partial_progressive(partial: &[u8]) -> Result<DynamicImage, image::ImageError> {
    if let Ok(img) = image::load_from_memory(partial) {
        return Ok(img);
    }

    // Some decoders require an explicit EOI marker for truncated streams.
    if partial.len() >= 2 && !(partial[partial.len() - 2] == 0xFF && partial[partial.len() - 1] == 0xD9) {
        let mut patched = Vec::with_capacity(partial.len() + 2);
        patched.extend_from_slice(partial);
        patched.extend_from_slice(&[0xFF, 0xD9]);
        return image::load_from_memory(&patched);
    }

    image::load_from_memory(partial)
}

#[derive(Debug)]
struct PngInspect {
    width: u32,
    height: u32,
    is_interlaced: bool,
}

fn inspect_png(bytes: &[u8]) -> Option<PngInspect> {
    // PNG signature + IHDR chunk header + IHDR payload + CRC.
    if bytes.len() < 33 || bytes[0..8] != [137, 80, 78, 71, 13, 10, 26, 10] {
        return None;
    }

    let ihdr_len = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if ihdr_len != 13 || &bytes[12..16] != b"IHDR" {
        return None;
    }

    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let interlace = bytes[28];

    Some(PngInspect {
        width,
        height,
        is_interlaced: interlace == 1,
    })
}

fn png_partial_read_bytes(png: &PngInspect, available_len: usize) -> usize {
    let estimated = ((png.width as u64 * png.height as u64) / 18) as usize;
    (estimated.max(16 * 1024)).min(available_len)
}

fn decode_partial_interlaced_png(partial: &[u8]) -> Result<DynamicImage, image::ImageError> {
    if let Ok(img) = image::load_from_memory(partial) {
        return Ok(img);
    }

    // Some decoders can recover if truncated PNGs are closed with IEND.
    const IEND_CHUNK: [u8; 12] = [0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130];
    let mut patched = Vec::with_capacity(partial.len() + IEND_CHUNK.len());
    patched.extend_from_slice(partial);
    patched.extend_from_slice(&IEND_CHUNK);
    image::load_from_memory(&patched)
}

#[derive(Debug)]
struct JpegInspect {
    is_progressive: bool,
    embedded_thumbnail_jpeg: Option<Vec<u8>>,
    width: Option<u32>,
    height: Option<u32>,
}

fn inspect_jpeg(bytes: &[u8]) -> Option<JpegInspect> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }

    let mut pos = 2usize;
    let mut is_progressive = false;
    let mut embedded_thumbnail_jpeg = None;
    let mut width = None;
    let mut height = None;

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

        // Standalone markers without payload.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            if marker == 0xD9 {
                break;
            }
            continue;
        }

        if pos + 2 > bytes.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        if seg_len < 2 || pos + seg_len > bytes.len() {
            break;
        }

        let data_start = pos + 2;
        let data_end = pos + seg_len;
        let data = &bytes[data_start..data_end];

        // SOF markers carry image dimensions.
        if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC5 | 0xC6 | 0xC7 | 0xC9 | 0xCA | 0xCB | 0xCD | 0xCE | 0xCF) && data.len() >= 5 {
            height = Some(u16::from_be_bytes([data[1], data[2]]) as u32);
            width = Some(u16::from_be_bytes([data[3], data[4]]) as u32);
        }

        // SOF2 means progressive JPEG.
        if marker == 0xC2 {
            is_progressive = true;
        }

        // APP1 Exif chunk may contain an embedded JPEG thumbnail.
        if marker == 0xE1 && embedded_thumbnail_jpeg.is_none() {
            embedded_thumbnail_jpeg = extract_exif_thumbnail_jpeg(data);
        }

        pos += seg_len;
        if marker == 0xDA {
            break;
        }
    }

    Some(JpegInspect {
        is_progressive,
        embedded_thumbnail_jpeg,
        width,
        height,
    })
}

fn progressive_partial_read_bytes(jpeg: &JpegInspect, available_len: usize) -> usize {
    let estimated = jpeg
        .width
        .zip(jpeg.height)
        .map(|(w, h)| ((w as u64 * h as u64) / 42) as usize)
        .unwrap_or(256 * 1024);

    (estimated.max(10 * 1024)).min(available_len)
}

fn extract_exif_thumbnail_jpeg(app1_data: &[u8]) -> Option<Vec<u8>> {
    // APP1 payload starts with "Exif\0\0" then TIFF data.
    if app1_data.len() < 6 || &app1_data[0..6] != b"Exif\0\0" {
        return None;
    }

    let tiff = &app1_data[6..];
    if tiff.len() < 8 {
        return None;
    }

    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };

    let ifd0_off = read_u32(tiff, 4, little)? as usize;
    if ifd0_off + 2 > tiff.len() {
        return None;
    }

    let ifd0_count = read_u16(tiff, ifd0_off, little)? as usize;
    let ifd0_next_ptr_off = ifd0_off + 2 + ifd0_count * 12;
    let ifd1_off = read_u32(tiff, ifd0_next_ptr_off, little)? as usize;
    if ifd1_off == 0 || ifd1_off + 2 > tiff.len() {
        return None;
    }

    let ifd1_count = read_u16(tiff, ifd1_off, little)? as usize;
    let mut jpeg_off: Option<usize> = None;
    let mut jpeg_len: Option<usize> = None;

    for i in 0..ifd1_count {
        let entry = ifd1_off + 2 + i * 12;
        if entry + 12 > tiff.len() {
            break;
        }

        let tag = read_u16(tiff, entry, little)?;
        let val = read_u32(tiff, entry + 8, little)? as usize;
        match tag {
            0x0201 => jpeg_off = Some(val), // JPEGInterchangeFormat
            0x0202 => jpeg_len = Some(val), // JPEGInterchangeFormatLength
            _ => {}
        }
    }

    let off = jpeg_off?;
    let len = jpeg_len?;
    let end = off.checked_add(len)?;
    if end > tiff.len() || len < 4 {
        return None;
    }

    let thumb = &tiff[off..end];
    if thumb[0] != 0xFF || thumb[1] != 0xD8 {
        return None;
    }

    Some(thumb.to_vec())
}

fn extract_tiff_embedded_jpeg_thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let (off, len) = find_tiff_embedded_jpeg_span(bytes)?;
    let end = off.checked_add(len)?;
    if end > bytes.len() || len < 4 {
        return None;
    }

    let thumb = &bytes[off..end];
    if thumb[0] != 0xFF || thumb[1] != 0xD8 {
        return None;
    }

    Some(thumb.to_vec())
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

    // Classic TIFF magic is 42. BigTIFF (43) is not handled here yet.
    if read_u16(bytes, 2, little)? != 42 {
        return None;
    }

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

        let Some((next_ifd, sub_ifds, jpeg_off, jpeg_len)) = parse_tiff_ifd(bytes, ifd_off, little) else {
            continue;
        };

        if next_ifd != 0 && next_ifd + 2 <= bytes.len() {
            queue.push_back(next_ifd);
        }
        for sub in sub_ifds {
            if sub != 0 && sub + 2 <= bytes.len() {
                queue.push_back(sub);
            }
        }

        if let (Some(off), Some(len)) = (jpeg_off, jpeg_len) {
            if len >= 4 && best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((off, len));
            }
        }
    }

    best
}

fn tiff_is_probably_raw_sensor(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }

    let little = match &bytes[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return false,
    };

    if read_u16(bytes, 2, little) != Some(42) {
        return false;
    }

    let Some(ifd0_off) = read_u32(bytes, 4, little).map(|v| v as usize) else {
        return false;
    };
    if ifd0_off + 2 > bytes.len() {
        return false;
    }

    let mut queue = VecDeque::from([ifd0_off]);
    let mut visited = HashSet::new();

    while let Some(ifd_off) = queue.pop_front() {
        if !visited.insert(ifd_off) || ifd_off + 2 > bytes.len() {
            continue;
        }

        let Some(count) = read_u16(bytes, ifd_off, little).map(|v| v as usize) else {
            continue;
        };
        let entries_off = ifd_off + 2;
        let entries_bytes = match count.checked_mul(12) {
            Some(v) => v,
            None => continue,
        };
        let next_ptr_off = match entries_off.checked_add(entries_bytes) {
            Some(v) => v,
            None => continue,
        };
        if next_ptr_off + 4 > bytes.len() {
            continue;
        }

        for i in 0..count {
            let entry = entries_off + i * 12;
            if entry + 12 > bytes.len() {
                break;
            }

            let Some(tag) = read_u16(bytes, entry, little) else {
                continue;
            };
            let field_type = read_u16(bytes, entry + 2, little).unwrap_or(0);
            let field_count = read_u32(bytes, entry + 4, little).unwrap_or(0) as usize;
            let value = read_u32(bytes, entry + 8, little).unwrap_or(0) as usize;

            // DNG and CFA-related tags strongly indicate sensor RAW data.
            if matches!(
                tag,
                0xC612 // DNGVersion
                    | 0xC613 // DNGBackwardVersion
                    | 0xC614 // UniqueCameraModel
                    | 0xC616 // CFAPlaneColor
                    | 0xC617 // CFALayout
                    | 0xC61A // BlackLevel
                    | 0xC61D // WhiteLevel
                    | 0x828D // CFARepeatPatternDim
                    | 0x828E // CFAPattern
            ) {
                return true;
            }

            // ExifIFD pointer and SubIFDs can hold RAW-related tags.
            if tag == 0x8769 {
                if value != 0 && value + 2 <= bytes.len() {
                    queue.push_back(value);
                }
            } else if tag == 0x014A {
                queue.extend(read_tiff_subifd_offsets(
                    bytes,
                    field_type,
                    field_count,
                    value,
                    little,
                ));
            }
        }

        if let Some(next_ifd) = read_u32(bytes, next_ptr_off, little).map(|v| v as usize)
            && next_ifd != 0
            && next_ifd + 2 <= bytes.len()
        {
            queue.push_back(next_ifd);
        }
    }

    false
}

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
    let entries_bytes = count.checked_mul(12)?;
    let next_ptr_off = entries_off.checked_add(entries_bytes)?;
    if next_ptr_off + 4 > bytes.len() {
        return None;
    }

    let mut jpeg_off: Option<usize> = None;
    let mut jpeg_len: Option<usize> = None;
    let mut sub_ifds = Vec::new();
    let mut compression: Option<u16> = None;
    let mut strip_offsets: Vec<usize> = Vec::new();
    let mut strip_counts: Vec<usize> = Vec::new();
    let mut tile_offsets: Vec<usize> = Vec::new();
    let mut tile_counts: Vec<usize> = Vec::new();

    for i in 0..count {
        let entry = entries_off + i * 12;
        if entry + 12 > bytes.len() {
            break;
        }

        let tag = read_u16(bytes, entry, little)?;
        let field_type = read_u16(bytes, entry + 2, little)?;
        let field_count = read_u32(bytes, entry + 4, little)? as usize;
        let value = read_u32(bytes, entry + 8, little)? as usize;

        match tag {
            0x0103 => compression = Some(value as u16),
            0x0201 => jpeg_off = Some(value),
            0x0202 => jpeg_len = Some(value),
            0x0111 => {
                strip_offsets = read_tiff_u32_values(bytes, field_type, field_count, value, little, 32)
                    .into_iter()
                    .map(|v| v as usize)
                    .collect();
            }
            0x0117 => {
                strip_counts = read_tiff_u32_values(bytes, field_type, field_count, value, little, 32)
                    .into_iter()
                    .map(|v| v as usize)
                    .collect();
            }
            0x0144 => {
                tile_offsets = read_tiff_u32_values(bytes, field_type, field_count, value, little, 32)
                    .into_iter()
                    .map(|v| v as usize)
                    .collect();
            }
            0x0145 => {
                tile_counts = read_tiff_u32_values(bytes, field_type, field_count, value, little, 32)
                    .into_iter()
                    .map(|v| v as usize)
                    .collect();
            }
            0x014A => {
                sub_ifds.extend(read_tiff_subifd_offsets(
                    bytes,
                    field_type,
                    field_count,
                    value,
                    little,
                ));
            }
            _ => {}
        }
    }

    // Many DNG/RAW previews are JPEG-compressed TIFF IFDs with strip/tile
    // offset+bytecount tags instead of JPEGInterchangeFormat tags.
    if jpeg_off.is_none() || jpeg_len.is_none() {
        if matches!(compression, Some(6 | 7)) {
            if let (Some(off), Some(len)) = (strip_offsets.first(), strip_counts.first()) {
                jpeg_off = Some(*off);
                jpeg_len = Some(*len);
            } else if let (Some(off), Some(len)) = (tile_offsets.first(), tile_counts.first()) {
                jpeg_off = Some(*off);
                jpeg_len = Some(*len);
            }
        }
    }

    let next_ifd = read_u32(bytes, next_ptr_off, little)? as usize;
    Some((next_ifd, sub_ifds, jpeg_off, jpeg_len))
}

fn read_tiff_subifd_offsets(
    bytes: &[u8],
    field_type: u16,
    field_count: usize,
    value: usize,
    little: bool,
) -> Vec<usize> {
    if field_count == 0 {
        return Vec::new();
    }

    // SubIFDs are typically LONG values (type=4). Keep this narrow and safe.
    if field_type != 4 {
        return Vec::new();
    }

    if field_count == 1 {
        return vec![value];
    }

    let count = field_count.min(32);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = value + i * 4;
        let Some(v) = read_u32(bytes, off, little) else {
            break;
        };
        out.push(v as usize);
    }
    out
}

fn read_tiff_u32_values(
    bytes: &[u8],
    field_type: u16,
    field_count: usize,
    value: usize,
    little: bool,
    max_count: usize,
) -> Vec<u32> {
    let count = field_count.min(max_count);
    if count == 0 {
        return Vec::new();
    }

    match field_type {
        // SHORT
        3 => {
            if field_count <= 2 {
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    let o = value + i * 2;
                    let Some(v) = read_u16(bytes, o, little) else {
                        break;
                    };
                    out.push(v as u32);
                }
                out
            } else {
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    let o = value + i * 2;
                    let Some(v) = read_u16(bytes, o, little) else {
                        break;
                    };
                    out.push(v as u32);
                }
                out
            }
        }
        // LONG
        4 => {
            if field_count == 1 {
                vec![value as u32]
            } else {
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    let o = value + i * 4;
                    let Some(v) = read_u32(bytes, o, little) else {
                        break;
                    };
                    out.push(v);
                }
                out
            }
        }
        _ => Vec::new(),
    }
}

fn read_u16(buf: &[u8], off: usize, little: bool) -> Option<u16> {
    let b0 = *buf.get(off)?;
    let b1 = *buf.get(off + 1)?;
    Some(if little {
        u16::from_le_bytes([b0, b1])
    } else {
        u16::from_be_bytes([b0, b1])
    })
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

fn orientation_name(orientation: Orientation) -> &'static str {
    match orientation {
        Orientation::NoTransforms => "no_transforms",
        Orientation::Rotate90 => "rotate90",
        Orientation::Rotate180 => "rotate180",
        Orientation::Rotate270 => "rotate270",
        Orientation::FlipHorizontal => "flip_horizontal",
        Orientation::FlipVertical => "flip_vertical",
        Orientation::Rotate90FlipH => "rotate90_flip_horizontal",
        Orientation::Rotate270FlipH => "rotate270_flip_horizontal",
    }
}

/// Process a single batch item for the general still-image case.
pub async fn process_item(item: &ItemRequest, profile: &ThumbnailProfile) -> ItemResult {
    let started = Instant::now();
    let developer_mode = app_config().developer_mode;
    let mut request_state = ThumbnailRequestState::new(item);

    let Some(url) = source_url(&item.source) else {
        return ItemResult {
            id: item.id.clone(),
            url: None,
            source_meta: None,
            thumbnail: None,
            media: None,
            media_type: None,
            extension: None,
            job_status: None,
            developer: None,
            error: Some("unsupported source type".into()),
        };
    };

    // -----------------------------------------------------------------------
    // Pre-request cache check
    //
    // Derive a preliminary cache key from the raw input URL (stripping auth
    // query params and normalising scheme/host) — this is possible *before*
    // any HTTP request because we don't need the post-redirect final URL for
    // the common non-redirecting case.
    //
    // If the cache holds a result we extract the upstream ETag stored in
    // source_meta and use it as a conditional GET header.  If the upstream
    // returns 304 our cached copy is confirmed fresh and we return it as
    // `Cached` without decoding a single byte.  If the upstream returns 200
    // the cached copy is stale and we fall through to reprocess.
    //
    // If there is no cache hit we fall back to the caller-supplied ETag,
    // which drives the `not_modified` path (the caller already has the
    // current result).
    //
    // Edge-case: a URL that redirects to a different final URL will miss on
    // the preliminary key for the first request, then land correctly on
    // subsequent ones once stored under the canonical final URL.
    // -----------------------------------------------------------------------
    let preliminary_key = crate::source::canonical_url_for(url);

    let (cache_hit_result, cache_hit_etag): (Option<ItemResult>, Option<String>) =
        match preliminary_key.as_deref() {
            Some(key) => {
                match crate::cache::cache().get(key).await {
                    Some(hit) => match serde_json::from_slice::<ItemResult>(&hit.data) {
                        Ok(cached) => {
                            let etag = cached.source_meta.as_ref().and_then(|m| m.etag.clone());
                            crate::cache::cache().record_access(crate::cache::CacheAccess {
                                cache_key: key.to_string(),
                                result: crate::cache::AccessResult::Hit,
                            }).await;
                            (Some(cached), etag)
                        }
                        Err(_) => {
                            // Schema mismatch after a deploy — treat as a miss and reprocess.
                            crate::cache::cache().record_access(crate::cache::CacheAccess {
                                cache_key: key.to_string(),
                                result: crate::cache::AccessResult::Miss,
                            }).await;
                            (None, None)
                        }
                    },
                    None => {
                        crate::cache::cache().record_access(crate::cache::CacheAccess {
                            cache_key: key.to_string(),
                            result: crate::cache::AccessResult::Miss,
                        }).await;
                        (None, None)
                    }
                }
            }
            None => (None, None),
        };

    // Use our stored upstream etag for the conditional if we have a cached
    // result; otherwise use the caller-supplied etag (not_modified path).
    let conditional = if cache_hit_etag.is_some() {
        parse_conditional_request(cache_hit_etag.as_deref())
    } else {
        parse_conditional_request(item.etag.as_deref())
    };

    let fetch_started = Instant::now();
    let remote = match fetch_remote_prefix(url, REMOTE_PREFIX_READ_BYTES, conditional.as_ref()).await {
        Ok(v) => v,
        Err(err) => {
            return ItemResult {
                id: item.id.clone(),
                url: Some(url.to_string()),
                source_meta: None,
                thumbnail: None,
                media: None,
                developer: None,
                error: Some(err),
                ..Default::default()
            }
        }
    };
    let download_time_secs = fetch_started.elapsed().as_secs_f64();

    let meta = remote.meta.clone();
    let make_developer = |thumbnail_size: u64, job_data: u64, job_strategy: Option<String>, width: Option<u32>, height: Option<u32>| {
        if !developer_mode {
            return None;
        }
        Some(build_developer_data(
            url,
            &remote.headers,
            &meta,
            thumbnail_size,
            job_data,
            job_strategy,
            width,
            height,
            download_time_secs,
            started.elapsed().as_secs_f64(),
        ))
    };

    if remote.not_modified {
        if let Some(mut cached) = cache_hit_result {
            // Upstream confirmed our cached copy is still fresh (our stored
            // etag matched).  Serve directly from cache.
            cached.job_status = Some(JobStatus::Cached);
            return cached;
        } else {
            // Upstream confirmed the resource hasn't changed since the
            // *caller's* etag.  Return a minimal result; no thumbnail needed.
            return ItemResult {
                id: item.id.clone(),
                url: Some(url.to_string()),
                source_meta: Some(meta.clone()),
                thumbnail: None,
                media: None,
                media_type: meta.file_kind.as_ref().map(|k| k.media_type),
                extension: meta.file_kind.as_ref().map(|k| k.extension.clone()),
                job_status: Some(JobStatus::NotModified),
                developer: make_developer(0, 0, None, None, None),
                error: None,
            };
        }
    }

    request_state.observe_prefix(&remote.prefix_bytes, &meta);

    if !item.ops.thumbnail {
        return ItemResult {
            id: item.id.clone(),
            url: Some(url.to_string()),
            source_meta: Some(meta.clone()),
            thumbnail: None,
            media: None,
            media_type: None,
            extension: None,
            job_status: None,
            developer: make_developer(0, remote.prefix_bytes.len() as u64, None, None, None),
            error: None,
        };
    }

    let mut stream_bytes_read = remote.prefix_bytes.len() as u64;

    let thumb = if remote.is_probably_jpeg() {
        match render_thumbnail_from_bytes_with_stream_bytes(&remote.prefix_bytes, profile, stream_bytes_read) {
            Ok((jpeg, info)) if info.decode_strategy != DecodeStrategy::FullImage.as_str() || remote.prefix_is_complete() => jpeg,
            Ok(_) | Err(_) => {
                let full = match fetch_url_full(url).await {
                    Ok(v) => v,
                    Err(err) => {
                        return ItemResult {
                            id: item.id.clone(),
                            url: Some(url.to_string()),
                            source_meta: Some(meta.clone()),
                            thumbnail: None,
                            media: None,
                            developer: make_developer(0, stream_bytes_read, None, None, None),
                            error: Some(err),
                            ..Default::default()
                        }
                    }
                };

                request_state.switch_to_streaming_mode();
                request_state.note_stream_bytes(full.len());
                stream_bytes_read = stream_bytes_read.saturating_add(full.len() as u64);

                match render_thumbnail_from_bytes_with_stream_bytes(&full, profile, stream_bytes_read) {
                    Ok((jpeg, _info)) => jpeg,
                    Err(err) => {
                        if let Some(dispatched) = crate::dispatch::try_dispatch_tier2(item, profile, &request_state).await {
                            return dispatched;
                        }
                        return ItemResult {
                            id: item.id.clone(),
                            url: Some(url.to_string()),
                            source_meta: Some(meta.clone()),
                            thumbnail: None,
                            media: None,
                            developer: make_developer(0, stream_bytes_read, None, None, None),
                            error: Some(err),
                            ..Default::default()
                        };
                    }
                }
            }
        }
    } else {
        let embedded = match try_fetch_remote_tiff_embedded_jpeg(url, &remote).await {
            Ok(v) => v,
            Err(err) => {
                return ItemResult {
                    id: item.id.clone(),
                    url: Some(url.to_string()),
                    source_meta: Some(meta.clone()),
                    thumbnail: None,
                    media: None,
                    developer: make_developer(0, stream_bytes_read, None, None, None),
                    error: Some(err),
                    ..Default::default()
                }
            }
        };

        if let Some((embedded_jpeg, extra_read)) = embedded {
            stream_bytes_read = stream_bytes_read.saturating_add(extra_read);

            if let Ok(img) = image::load_from_memory(&embedded_jpeg) {
                match render_thumbnail_from_dynamic_image_with_stream_bytes(
                    img,
                    profile,
                    stream_bytes_read,
                    DecodeStrategy::EmbeddedJpegThumbnail.as_str(),
                ) {
                    Ok((jpeg, _info)) => {
                        let thumb_size = jpeg.len() as u64;
                        return ItemResult {
                            id: item.id.clone(),
                            url: Some(url.to_string()),
                            source_meta: Some(meta.clone()),
                            thumbnail: Some(jpeg),
                            media: None,
                            developer: make_developer(
                                thumb_size,
                                stream_bytes_read,
                                Some(DecodeStrategy::EmbeddedJpegThumbnail.as_str().to_string()),
                                Some(profile.width),
                                Some(profile.height),
                            ),
                            error: None,
                            ..Default::default()
                        };
                    }
                    Err(err) => {
                        return ItemResult {
                            id: item.id.clone(),
                            url: Some(url.to_string()),
                            source_meta: Some(meta.clone()),
                            thumbnail: None,
                            media: None,
                            developer: make_developer(0, stream_bytes_read, None, None, None),
                            error: Some(err),
                            ..Default::default()
                        };
                    }
                }
            }
        }

        if let Ok((jpeg, info)) = render_thumbnail_from_bytes_with_stream_bytes(
            &remote.prefix_bytes,
            profile,
            stream_bytes_read,
        ) {
            if info.decode_strategy != DecodeStrategy::FullImage.as_str() || remote.prefix_is_complete() {
                let thumb_size = jpeg.len() as u64;
                return ItemResult {
                    id: item.id.clone(),
                    url: Some(url.to_string()),
                    source_meta: Some(meta.clone()),
                    thumbnail: Some(jpeg),
                    media: None,
                    developer: make_developer(
                        thumb_size,
                        stream_bytes_read,
                        Some(info.decode_strategy),
                        Some(profile.width),
                        Some(profile.height),
                    ),
                    error: None,
                    ..Default::default()
                };
            }
        }

        let full = match fetch_url_full(url).await {
            Ok(v) => v,
            Err(err) => {
                return ItemResult {
                    id: item.id.clone(),
                    url: Some(url.to_string()),
                    source_meta: Some(meta.clone()),
                    thumbnail: None,
                    media: None,
                    developer: make_developer(0, stream_bytes_read, None, None, None),
                    error: Some(err),
                    ..Default::default()
                }
            }
        };

        request_state.switch_to_streaming_mode();
        request_state.note_stream_bytes(full.len());
        stream_bytes_read = stream_bytes_read.saturating_add(full.len() as u64);

        match render_thumbnail_from_bytes_with_stream_bytes(&full, profile, stream_bytes_read) {
            Ok((jpeg, _info)) => jpeg,
            Err(err) => {
                if let Some(dispatched) = crate::dispatch::try_dispatch_tier2(item, profile, &request_state).await {
                    return dispatched;
                }
                return ItemResult {
                    id: item.id.clone(),
                    url: Some(url.to_string()),
                    source_meta: Some(meta.clone()),
                    thumbnail: None,
                    media: None,
                    developer: make_developer(0, stream_bytes_read, None, None, None),
                    error: Some(err),
                    ..Default::default()
                };
            }
        }
    };

    let thumb_size = thumb.len() as u64;
    let result = ItemResult {
        id: item.id.clone(),
        url: Some(url.to_string()),
        source_meta: Some(meta.clone()),
        thumbnail: Some(thumb),
        media: None,
        media_type: meta.file_kind.as_ref().map(|k| k.media_type),
        extension: meta.file_kind.as_ref().map(|k| k.extension.clone()),
        job_status: Some(JobStatus::Success),
        developer: make_developer(
            thumb_size,
            stream_bytes_read,
            None,
            Some(profile.width),
            Some(profile.height),
        ),
        error: None,
    };

    // -----------------------------------------------------------------------
    // Cache store
    //
    // Serialise and write under the authoritative post-redirect canonical URL.
    // If the URL was redirected the preliminary key (raw input URL, stripped)
    // will differ from the final key; store under both so the next request
    // using the same raw URL hits the cache pre-request without needing to
    // follow the redirect again.
    // -----------------------------------------------------------------------
    if let Some(final_key) = meta.cache_key.as_deref() {
        if let Ok(bytes) = serde_json::to_vec(&result) {
            crate::cache::cache().put(final_key, bytes.clone()).await;
            // Also index under preliminary key when the redirect changed the URL.
            if preliminary_key.as_deref() != Some(final_key) {
                if let Some(ref prelim) = preliminary_key {
                    crate::cache::cache().put(prelim, bytes).await;
                }
            }
        }
    }

    result
}

fn build_developer_data(
    url: &str,
    headers: &std::collections::HashMap<String, String>,
    meta: &SourceMetadata,
    thumbnail_size: u64,
    job_data: u64,
    job_strategy: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    download_time_secs: f64,
    process_time_secs: f64,
) -> DeveloperData {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    DeveloperData {
        fetch_headers: Some(headers.clone()),
        media_log: Some(MediaLogData {
            timestamp: format!("{ts}"),
            url: url.to_string(),
            thumbnail_size,
            job_data,
            job_strategy,
            job_image_buffer_width: width,
            job_image_buffer_height: height,
            download_time_secs,
            process_time_secs,
            file_length: meta.content_length,
            media_type: meta.file_kind.as_ref().map(|k| k.media_type),
            extension: meta.file_kind.as_ref().map(|k| k.extension.clone()),
        }),
    }
}

fn source_url(source: &SourceRef) -> Option<&str> {
    match source {
        SourceRef::Url { url } => Some(url.as_str()),
    }
}

fn looks_like_tiff_container(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && ((bytes[0] == b'I' && bytes[1] == b'I' && bytes[2] == 42 && bytes[3] == 0)
            || (bytes[0] == b'M' && bytes[1] == b'M' && bytes[2] == 0 && bytes[3] == 42))
}

async fn try_fetch_remote_tiff_embedded_jpeg(
    url: &str,
    remote: &RemotePrefix,
) -> Result<Option<(Vec<u8>, u64)>, String> {
    if !looks_like_tiff_container(&remote.prefix_bytes)
        && !remote
            .meta
            .magic_mime
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("image/tiff"))
            .unwrap_or(false)
    {
        return Ok(None);
    }

    let Some((off, len)) = find_tiff_embedded_jpeg_span(&remote.prefix_bytes) else {
        return Ok(None);
    };
    if len == 0 || len > MAX_DOWNLOAD_BYTES {
        return Ok(None);
    }

    let end = match off.checked_add(len) {
        Some(v) => v,
        None => return Ok(None),
    };

    if end <= remote.prefix_bytes.len() {
        let thumb = remote.prefix_bytes[off..end].to_vec();
        if thumb.starts_with(&[0xFF, 0xD8]) {
            return Ok(Some((thumb, 0)));
        }
        return Ok(None);
    }

    if !remote.meta.accepts_ranges {
        return Ok(None);
    }

    let range_start = off as u64;
    let range_end = (end - 1) as u64;
    let bytes = fetch_url_range(url, range_start, range_end).await?;
    if bytes.len() != len || !bytes.starts_with(&[0xFF, 0xD8]) {
        return Ok(None);
    }

    Ok(Some((bytes, len as u64)))
}

struct RemotePrefix {
    prefix_bytes: Vec<u8>,
    meta: SourceMetadata,
    headers: std::collections::HashMap<String, String>,
    complete: bool,
    not_modified: bool,
}

impl RemotePrefix {
    fn prefix_is_complete(&self) -> bool {
        self.complete
    }

    fn is_probably_jpeg(&self) -> bool {
        if let Some(magic) = &self.meta.magic_mime {
            if magic.eq_ignore_ascii_case("image/jpeg") {
                return true;
            }
        }

        self.meta
            .content_type
            .as_deref()
            .map(|v| v.eq_ignore_ascii_case("image/jpeg"))
            .unwrap_or(false)
    }
}

fn parse_conditional_request(etag: Option<&str>) -> Option<ConditionalRequest> {
    let raw = etag?.trim();
    if raw.is_empty() {
        return None;
    }

    if raw.len() >= 2 {
        let (prefix, value) = raw.split_at(1);
        if value.is_empty() {
            return None;
        }

        return match prefix {
            "E" => Some(ConditionalRequest::IfNoneMatch(value.to_string())),
            "M" => Some(ConditionalRequest::IfModifiedSince(value.to_string())),
            _ => Some(ConditionalRequest::IfNoneMatch(raw.to_string())),
        };
    }

    Some(ConditionalRequest::IfNoneMatch(raw.to_string()))
}

fn encode_source_etag(headers: &std::collections::HashMap<String, String>) -> Option<String> {
    if let Some(etag) = header_string_map(headers, "etag").filter(|v| !v.trim().is_empty()) {
        return Some(format!("E{etag}"));
    }

    header_string_map(headers, "last-modified")
        .filter(|v| !v.trim().is_empty())
        .map(|v| format!("M{v}"))
}

async fn fetch_remote_prefix(
    url: &str,
    max_bytes: usize,
    conditional: Option<&ConditionalRequest>,
) -> Result<RemotePrefix, String> {
    let pref = http_source::fetch_prefix(url, max_bytes, conditional).await?;
    let content_length = parse_content_length_map(&pref.headers);

    if let Some(total) = content_length {
        if total > MAX_DOWNLOAD_BYTES as u64 {
            return Err("source is too large".into());
        }
    }

    if pref.bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err("source is too large".into());
    }

    let complete = if let Some(total) = content_length {
        pref.bytes.len() as u64 >= total
    } else {
        pref.stream_finished && pref.status != 206
    };

    let magic_mime = infer::get(&pref.bytes).map(|k| k.mime_type().to_string());

    let content_type_str = header_string_map(&pref.headers, "content-type")
        .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
        .filter(|v| !v.is_empty());
    let file_kind = crate::media::sniff_file_kind(
        &pref.bytes,
        content_type_str.as_deref(),
    );

    let content_length = parse_total_content_length_map(&pref.headers).or(content_length);

    // Build the canonical URL from the post-redirect final URL, falling back
    // to the original request URL if the transport didn't capture it.
    let canonical_url = crate::source::canonical_url_for(
        pref.final_url.as_deref().unwrap_or(url),
    );
    // Cache key is currently identical to the canonical URL.
    // Future: incorporate a scoped account ID or hash.
    let cache_key = canonical_url.clone();

    let meta = SourceMetadata {
        content_type: content_type_str,
        magic_mime,
        content_length,
        etag: encode_source_etag(&pref.headers),
        last_modified: header_string_map(&pref.headers, "last-modified"),
        accepts_ranges: header_string_map(&pref.headers, "accept-ranges")
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false),
        file_kind,
        canonical_url,
        cache_key,
    };

    Ok(RemotePrefix {
        prefix_bytes: pref.bytes,
        meta,
        headers: pref.headers,
        complete,
        not_modified: pref.status == 304,
    })
}

async fn fetch_url_full(url: &str) -> Result<Vec<u8>, String> {
    let (bytes, headers) = http_source::fetch_full(url).await?;

    if parse_content_length_map(&headers).is_some_and(|n| n > MAX_DOWNLOAD_BYTES as u64) {
        return Err("source is too large".into());
    }

    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err("source is too large".into());
    }

    Ok(bytes)
}

async fn fetch_url_range(url: &str, start: u64, end_inclusive: u64) -> Result<Vec<u8>, String> {
    let (bytes, headers) = http_source::fetch_range(url, start, end_inclusive).await?;

    if parse_content_length_map(&headers).is_some_and(|n| n > MAX_DOWNLOAD_BYTES as u64) {
        return Err("source is too large".into());
    }
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err("source is too large".into());
    }

    Ok(bytes)
}

fn parse_content_length_map(headers: &std::collections::HashMap<String, String>) -> Option<u64> {
    headers.get("content-length").and_then(|v| v.parse::<u64>().ok())
}

fn parse_total_content_length_map(headers: &std::collections::HashMap<String, String>) -> Option<u64> {
    let cr = headers.get("content-range")?;
    let total = cr.rsplit('/').next()?;
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

fn header_string_map(headers: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    headers.get(key).cloned()
}

/// Mutable processing buffer for post-decode image operations.
///
/// This is the single place where image transforms are applied so future
/// filters and color stages can be added without changing handler call sites.
struct ProcessBuffer {
    img: DynamicImage,
}

impl ProcessBuffer {
    fn new(img: DynamicImage) -> Self {
        Self { img }
    }

    fn composite_transparency_over_background(&mut self) {
        let rgba = self.img.to_rgba8();
        if !image_has_transparency(&rgba) {
            self.img = DynamicImage::ImageRgba8(rgba);
            return;
        }

        let (w, h) = rgba.dimensions();
        let Some(bg) = load_transparency_background().map(|base| fit_background_to(base, w, h)) else {
            // PNG failed to load — fall back to white.
            let flattened = flatten_alpha(&rgba, [255, 255, 255]);
            self.img = DynamicImage::ImageRgb8(flattened);
            return;
        };

        let mut out = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let fg = *rgba.get_pixel(x, y);
                let bg_px = *bg.get_pixel(x, y);
                let rgb = blend_over_bg(fg, [bg_px[0], bg_px[1], bg_px[2]]);
                out.put_pixel(x, y, Rgba([rgb[0], rgb[1], rgb[2], 255]));
            }
        }

        // All pixels now have alpha=255 after compositing; flatten to RGB.
        let flattened = flatten_alpha(&out, [255, 255, 255]);
        self.img = DynamicImage::ImageRgb8(flattened);
    }

    /// Placeholder for future color and filtering passes.
    fn apply_color_pipeline(&mut self, pixel_art_mode: bool, vignette_strength: f32) {
        // Downscaled images benefit from a mild unsharp pass to recover edge
        // definition lost during resize.
        if !pixel_art_mode {
            self.img = self.img.unsharpen(0.85, 2);
        }

        let strength = if pixel_art_mode {
            (vignette_strength * 0.6).clamp(0.0, 1.0)
        } else {
            vignette_strength.clamp(0.0, 1.0)
        };
        self.apply_soft_vignette(strength, 0.62, 1.25);
    }

    /// Apply a soft dark vignette around the edges.
    ///
    /// Parameters:
    /// - `strength`: max darkening near the far edges, in 0..1.
    /// - `inner`: radius where darkening begins (normalized ellipse radius).
    /// - `outer`: radius where full vignette strength is reached.
    fn apply_soft_vignette(&mut self, strength: f32, inner: f32, outer: f32) {
        let mut rgba = self.img.to_rgba8();
        let (w, h) = rgba.dimensions();
        if w < 2 || h < 2 {
            self.img = DynamicImage::ImageRgba8(rgba);
            return;
        }

        let cx = (w as f32 - 1.0) * 0.5;
        let cy = (h as f32 - 1.0) * 0.5;
        let rx = cx.max(1.0);
        let ry = cy.max(1.0);
        let inv_span = 1.0 / (outer - inner).max(1e-6);

        for y in 0..h {
            for x in 0..w {
                let dx = (x as f32 - cx) / rx;
                let dy = (y as f32 - cy) / ry;
                let r = (dx * dx + dy * dy).sqrt();

                let t = ((r - inner) * inv_span).clamp(0.0, 1.0);
                let smooth = t * t * (3.0 - 2.0 * t);

                // Luminance-adaptive vignette:
                // - bright pixels get a little relief (less darkening)
                // - dark pixels get more edge emphasis (more darkening)
                let lum = rgb_luma(*rgba.get_pixel(x, y));
                let shadow_boost = (1.0 - lum).powf(1.25);
                let highlight_relief = lum.powf(1.10);
                let adaptive_strength = (strength * (1.0 + 0.45 * shadow_boost - 0.35 * highlight_relief))
                    .clamp(0.0, 0.95);

                let weight = 1.0 - adaptive_strength * smooth;

                let p = rgba.get_pixel_mut(x, y);
                p[0] = ((p[0] as f32 * weight).round()).clamp(0.0, 255.0) as u8;
                p[1] = ((p[1] as f32 * weight).round()).clamp(0.0, 255.0) as u8;
                p[2] = ((p[2] as f32 * weight).round()).clamp(0.0, 255.0) as u8;
            }
        }

        self.img = DynamicImage::ImageRgba8(rgba);
    }

    /// Scale and center-crop to fill the target aspect ratio.
    ///
    /// Returns whether the operation upscaled the source.
    fn fill_crop(&mut self, target_w: u32, target_h: u32, use_photo_filter_for_upscale: bool) -> bool {
        let src = self.img.to_rgba8();
        let (src_w, src_h) = src.dimensions();

        if src_w == 0 || src_h == 0 || target_w == 0 || target_h == 0 {
            self.img = DynamicImage::ImageRgba8(RgbaImage::new(target_w.max(1), target_h.max(1)));
            return false;
        }

        // Deliberately overscale by 10% before cropping so we keep a bit more
        // zoom than exact fill and avoid edge-adjacent composition artifacts.
        let overscale_w = ((target_w as f32) * 1.10).ceil() as u32;
        let overscale_h = ((target_h as f32) * 1.10).ceil() as u32;

        let scale_w = overscale_w as f32 / src_w as f32;
        let scale_h = overscale_h as f32 / src_h as f32;
        let scale = scale_w.max(scale_h);
        let upscaled = scale > 1.0;

        let new_w = ((src_w as f32) * scale).ceil() as u32;
        let new_h = ((src_h as f32) * scale).ceil() as u32;

        // If the source is already close to the computed overscale target,
        // avoid an extra resample pass and crop directly.
        let near_target_w = src_w.abs_diff(new_w) <= 8;
        let near_target_h = src_h.abs_diff(new_h) <= 8;
        let near_target_scale = (scale - 1.0).abs() <= 0.06;
        let skip_resize = near_target_w && near_target_h && near_target_scale;

        // Pixel art and low-res assets look better with nearest-neighbor when
        // scaling up. Keep Lanczos for downscaling or for photographic sources
        // (e.g. embedded JPEG thumbnails from camera files).
        let resized = if skip_resize {
            src
        } else {
            let filter = if scale > 1.0 && !use_photo_filter_for_upscale {
                FilterType::Nearest
            } else {
                FilterType::Lanczos3
            };
            resize(&src, new_w, new_h, filter)
        };
        let extra_w = resized.width().saturating_sub(target_w);
        let extra_h = resized.height().saturating_sub(target_h);

        // Horizontal crop stays centered.
        let x = extra_w / 2;
        // Vertical crop is biased toward the top: start 25% into the extra height.
        let y = (extra_h as f32 * 0.25).floor() as u32;

        let cropped = crop_imm(&resized, x, y, target_w, target_h).to_image();
        self.img = DynamicImage::ImageRgba8(cropped);
        upscaled && !skip_resize
    }

    fn encode_jpeg(&self, quality: u8) -> Result<Vec<u8>, String> {
        let rgba = self.img.to_rgba8();
        // After compositing the image should already be fully opaque, but
        // flatten_alpha handles any residual alpha with a white fallback.
        let rgb = flatten_alpha(&rgba, [255, 255, 255]);

        let width = rgb.width() as usize;
        let height = rgb.height() as usize;
        let pixels = rgb.into_raw();

        let mut enc = Compress::new(ColorSpace::JCS_RGB);
        enc.set_size(width, height);
        enc.set_quality(quality as f32);
        enc.set_smoothing_factor(20);
        // Force 4:2:0 chroma subsampling for smaller files.
        enc.set_chroma_sampling_pixel_sizes((2, 2), (2, 2));
        enc.set_optimize_coding(true);

        let mut started = enc
            .start_compress(Vec::new())
            .map_err(|e| format!("jpeg start_compress failed: {e}"))?;

        // Tiny vanity marker for generated thumbnails.
        started.write_marker(Marker::COM, b"thumbrella");

        started
            .write_scanlines(&pixels)
            .map_err(|e| format!("jpeg write_scanlines failed: {e}"))?;
        started
            .finish()
            .map_err(|e| format!("jpeg finish failed: {e}"))
    }
}

fn load_transparency_background() -> Option<&'static RgbaImage> {
    TRANSPARENCY_BACKGROUND
        .get_or_init(|| {
            image::load_from_memory(include_bytes!("../../../media/background.png"))
                .ok()
                .map(|img| img.to_rgba8())
        })
        .as_ref()
}

fn fit_background_to(bg: &RgbaImage, target_w: u32, target_h: u32) -> RgbaImage {
    if target_w == 0 || target_h == 0 {
        return RgbaImage::new(target_w.max(1), target_h.max(1));
    }

    let (src_w, src_h) = bg.dimensions();
    if src_w == 0 || src_h == 0 {
        return RgbaImage::new(target_w, target_h);
    }

    let scale_w = target_w as f32 / src_w as f32;
    let scale_h = target_h as f32 / src_h as f32;
    let scale = scale_w.max(scale_h);
    let new_w = ((src_w as f32) * scale).ceil() as u32;
    let new_h = ((src_h as f32) * scale).ceil() as u32;

    let resized = resize(bg, new_w.max(1), new_h.max(1), FilterType::Lanczos3);
    let x = resized.width().saturating_sub(target_w) / 2;
    let y = resized.height().saturating_sub(target_h) / 2;
    crop_imm(&resized, x, y, target_w, target_h).to_image()
}

fn image_has_transparency(rgba: &RgbaImage) -> bool {
    rgba.pixels().any(|p| p[3] < 255)
}

#[inline]
fn rgb_luma(p: Rgba<u8>) -> f32 {
    // Rec. 709 luma weights.
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn flatten_alpha(rgba: &RgbaImage, bg: [u8; 3]) -> image::RgbImage {
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbImage::new(w, h);

    for y in 0..h {
        for x in 0..w {
            let p = rgba.get_pixel(x, y);
            let blended = blend_over_bg(*p, bg);
            out.put_pixel(x, y, image::Rgb(blended));
        }
    }

    out
}

fn blend_over_bg(px: Rgba<u8>, bg: [u8; 3]) -> [u8; 3] {
    let a = px[3] as u16;
    let inv = 255u16.saturating_sub(a);

    let r = ((px[0] as u16 * a) + (bg[0] as u16 * inv) + 127) / 255;
    let g = ((px[1] as u16 * a) + (bg[1] as u16 * inv) + 127) / 255;
    let b = ((px[2] as u16 * a) + (bg[2] as u16 * inv) + 127) / 255;

    [r as u8, g as u8, b as u8]
}
