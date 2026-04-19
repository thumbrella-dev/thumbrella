//! Core synchronous processing pipeline for common still-image inputs.
//!
//! Design rule: every input image goes through this same decode -> process ->
//! encode path. Even if a source contains an embedded thumbnail, we still run
//! it through post-processing so no output pixel bypasses the pipeline.

use image::imageops::{FilterType, crop_imm, resize};
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba, RgbaImage, metadata::Orientation};
use mozjpeg::{ColorSpace, Compress, Marker};
use serde::Serialize;
use std::io::Cursor;
use crate::{ItemRequest, ItemResult, SourceMetadata, SourceRef, ThumbnailProfile, http_source};
use zip::read::ZipArchive;

const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
// Prefix read for remote sources should be large enough to cover common
// progressive partial decode windows without requiring a second fetch.
const REMOTE_PREFIX_READ_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy)]
enum DecodeStrategy {
    FullImage,
    EmbeddedJpegThumbnail,
    ProgressivePartial,
    OdtPackageThumbnail,
    DocxPackageThumbnail,
}

impl DecodeStrategy {
    fn as_str(self) -> &'static str {
        match self {
            DecodeStrategy::FullImage => "full_image",
            DecodeStrategy::EmbeddedJpegThumbnail => "embedded_jpeg_thumbnail",
            DecodeStrategy::ProgressivePartial => "progressive_partial",
            DecodeStrategy::OdtPackageThumbnail => "odt_package_thumbnail",
            DecodeStrategy::DocxPackageThumbnail => "docx_package_thumbnail",
        }
    }

    fn is_embedded_thumbnail(self) -> bool {
        matches!(self, DecodeStrategy::EmbeddedJpegThumbnail)
    }
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
    pub upscaled: bool,
    pub output_width: u32,
    pub output_height: u32,
    pub output_quality: u8,
}

/// Build source metadata for a local byte source.
pub fn metadata_from_local_bytes(bytes: &[u8], content_length: Option<u64>, last_modified: Option<String>) -> SourceMetadata {
    SourceMetadata {
        content_type: None,
        magic_mime: infer::get(bytes).map(|k| k.mime_type().to_string()),
        content_length,
        etag: None,
        last_modified,
        accepts_ranges: false,
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
    let (mut img, strategy) = decode_image_with_strategy(bytes)?;

    // Source-level metadata should come from the full image stream when that
    // stream is itself an image. For container-derived thumbnails (ODT/DOCX),
    // fall back to decoded image dimensions and no orientation transform.
    let source_probe = probe_source_image_info(bytes);
    let orientation = source_probe
        .as_ref()
        .map(|(_, _, o)| *o)
        .unwrap_or(Orientation::NoTransforms);

    // Let the image crate apply EXIF orientation transforms so camera images
    // land upright without custom orientation parsing logic.
    img.apply_orientation(orientation);

    let decoded_width = img.width();
    let decoded_height = img.height();
    let (source_width, source_height) = source_probe
        .map(|(w, h, _)| (w, h))
        .unwrap_or((decoded_width, decoded_height));

    let mut buf = ProcessBuffer::new(img);
    let scaled_up = buf.fill_crop(profile.width, profile.height);

    // Embedded EXIF thumbnails should not be penalized with the low-quality
    // "upscaled" profile. Treat them as regular renders for filtering/effects.
    let treat_as_upscaled = scaled_up && !strategy.is_embedded_thumbnail();
    buf.apply_color_pipeline(treat_as_upscaled);

    // Upscaled sources often have blocky low-detail content (sprites/icons).
    // A lower JPEG quality keeps output size bounded for those cases.
    let effective_quality = if treat_as_upscaled { 15 } else { profile.quality };

    let thumb = buf.encode_jpeg(effective_quality, profile.background)?;
    let info = RenderInfo {
        stream_bytes_read,
        decode_strategy: strategy.as_str().to_string(),
        input_width: source_width,
        input_height: source_height,
        decoded_width,
        decoded_height,
        source_orientation: orientation_name(orientation).to_string(),
        upscaled: treat_as_upscaled,
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

    estimated.clamp(10 * 1024, available_len)
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
    let Some(url) = source_url(&item.source) else {
        return ItemResult {
            id: item.id.clone(),
            source_meta: None,
            thumbnail: None,
            error: Some("unsupported source type".into()),
        };
    };

    let remote = match fetch_remote_prefix(url, REMOTE_PREFIX_READ_BYTES).await {
        Ok(v) => v,
        Err(err) => {
            return ItemResult {
                id: item.id.clone(),
                source_meta: None,
                thumbnail: None,
                error: Some(err),
            }
        }
    };

    let meta = remote.meta.clone();

    if !item.ops.thumbnail {
        return ItemResult {
            id: item.id.clone(),
            source_meta: Some(meta.clone()),
            thumbnail: None,
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
                            source_meta: Some(meta.clone()),
                            thumbnail: None,
                            error: Some(err),
                        }
                    }
                };

                stream_bytes_read = stream_bytes_read.saturating_add(full.len() as u64);

                match render_thumbnail_from_bytes_with_stream_bytes(&full, profile, stream_bytes_read) {
                    Ok((jpeg, _info)) => jpeg,
                    Err(err) => {
                        return ItemResult {
                            id: item.id.clone(),
                            source_meta: Some(meta.clone()),
                            thumbnail: None,
                            error: Some(err),
                        }
                    }
                }
            }
        }
    } else {
        let full = match fetch_url_full(url).await {
            Ok(v) => v,
            Err(err) => {
                return ItemResult {
                    id: item.id.clone(),
                    source_meta: Some(meta.clone()),
                    thumbnail: None,
                    error: Some(err),
                }
            }
        };

        stream_bytes_read = stream_bytes_read.saturating_add(full.len() as u64);

        match render_thumbnail_from_bytes_with_stream_bytes(&full, profile, stream_bytes_read) {
            Ok((jpeg, _info)) => jpeg,
            Err(err) => {
                return ItemResult {
                    id: item.id.clone(),
                    source_meta: Some(meta.clone()),
                    thumbnail: None,
                    error: Some(err),
                }
            }
        }
    };

    ItemResult {
        id: item.id.clone(),
        source_meta: Some(meta),
        thumbnail: Some(thumb),
        error: None,
    }
}

fn source_url(source: &SourceRef) -> Option<&str> {
    match source {
        SourceRef::Url { url } => Some(url.as_str()),
    }
}

struct RemotePrefix {
    prefix_bytes: Vec<u8>,
    meta: SourceMetadata,
    complete: bool,
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

async fn fetch_remote_prefix(url: &str, max_bytes: usize) -> Result<RemotePrefix, String> {
    let pref = http_source::fetch_prefix(url, max_bytes).await?;
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

    let content_length = parse_total_content_length_map(&pref.headers).or(content_length);
    let meta = SourceMetadata {
        content_type: header_string_map(&pref.headers, "content-type")
            .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
            .filter(|v| !v.is_empty()),
        magic_mime,
        content_length,
        etag: header_string_map(&pref.headers, "etag"),
        last_modified: header_string_map(&pref.headers, "last-modified"),
        accepts_ranges: header_string_map(&pref.headers, "accept-ranges")
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false),
    };

    Ok(RemotePrefix {
        prefix_bytes: pref.bytes,
        meta,
        complete,
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

    /// Placeholder for future color and filtering passes.
    fn apply_color_pipeline(&mut self, upscaled: bool) {
        // Downscaled images benefit from a mild unsharp pass to recover edge
        // definition lost during resize.
        if !upscaled {
            self.img = self.img.unsharpen(0.85, 2);
        }

        let strength = if upscaled { 0.15 } else { 0.25 };
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
    fn fill_crop(&mut self, target_w: u32, target_h: u32) -> bool {
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

        // Pixel art and low-res assets look better with nearest-neighbor when
        // scaling up. Keep Lanczos for downscaling photographic sources.
        let filter = if scale > 1.0 {
            FilterType::Nearest
        } else {
            FilterType::Lanczos3
        };
        let resized = resize(&src, new_w, new_h, filter);
        let extra_w = new_w.saturating_sub(target_w);
        let extra_h = new_h.saturating_sub(target_h);

        // Horizontal crop stays centered.
        let x = extra_w / 2;
        // Vertical crop is biased toward the top: start 25% into the extra height.
        let y = (extra_h as f32 * 0.25).floor() as u32;

        let cropped = crop_imm(&resized, x, y, target_w, target_h).to_image();
        self.img = DynamicImage::ImageRgba8(cropped);
        upscaled
    }

    fn encode_jpeg(&self, quality: u8, bg: [u8; 3]) -> Result<Vec<u8>, String> {
        let rgba = self.img.to_rgba8();
        let rgb = flatten_alpha(&rgba, bg);

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
