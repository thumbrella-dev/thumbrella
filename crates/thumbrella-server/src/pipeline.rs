//! Core synchronous processing pipeline for common still-image inputs.
//!
//! Design rule: every input image goes through this same decode -> process ->
//! encode path. Even if a source contains an embedded thumbnail, we still run
//! it through post-processing so no output pixel bypasses the pipeline.

use image::codecs::jpeg::JpegEncoder;
use image::imageops::{FilterType, crop_imm, resize};
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba, RgbaImage, metadata::Orientation};
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use serde::Serialize;
use std::io::Cursor;
use thumbrella_types::{ItemRequest, ItemResult, SourceMetadata, SourceRef, ThumbnailProfile};

const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;

/// Render summary produced by the image post-process pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct RenderInfo {
    pub input_width: u32,
    pub input_height: u32,
    pub source_orientation: String,
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
    let cursor = Cursor::new(bytes);
    let reader = ImageReader::new(cursor)
        .with_guessed_format()
        .map_err(|err| format!("failed to detect image format: {err}"))?;

    let mut decoder = reader
        .into_decoder()
        .map_err(|err| format!("failed to build image decoder: {err}"))?;

    let orientation = decoder
        .orientation()
        .map_err(|err| format!("failed reading image orientation: {err}"))?;

    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|err| format!("unsupported or invalid image: {err}"))?;

    // Let the image crate apply EXIF orientation transforms so camera images
    // land upright without custom orientation parsing logic.
    img.apply_orientation(orientation);

    let input_width = img.width();
    let input_height = img.height();

    let mut buf = ProcessBuffer::new(img);
    buf.apply_color_pipeline();
    buf.fill_crop(profile.width, profile.height);

    let thumb = buf.encode_jpeg(profile.quality, profile.background)?;
    let info = RenderInfo {
        input_width,
        input_height,
        source_orientation: orientation_name(orientation).to_string(),
        output_width: profile.width,
        output_height: profile.height,
        output_quality: profile.quality,
    };

    Ok((thumb, info))
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

    let (bytes, meta) = match fetch_url(url).await {
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

    if !item.ops.thumbnail {
        return ItemResult {
            id: item.id.clone(),
            source_meta: Some(meta),
            thumbnail: None,
            error: None,
        };
    }

    let thumb = match render_thumbnail_from_bytes(&bytes, profile) {
        Ok((jpeg, _info)) => jpeg,
        Err(err) => {
            return ItemResult {
                id: item.id.clone(),
                source_meta: Some(meta),
                thumbnail: None,
                error: Some(err),
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

async fn fetch_url(url: &str) -> Result<(Vec<u8>, SourceMetadata), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http and https URLs are supported".into());
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("upstream returned status {}", resp.status()));
    }

    let headers = resp.headers().clone();
    let content_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    if content_length.is_some_and(|n| n > MAX_DOWNLOAD_BYTES as u64) {
        return Err("source is too large".into());
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?
        .to_vec();

    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err("source is too large".into());
    }

    let magic_mime = infer::get(&bytes).map(|k| k.mime_type().to_string());
    let meta = SourceMetadata {
        content_type: header_string(&headers, CONTENT_TYPE)
            .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
            .filter(|v| !v.is_empty()),
        magic_mime,
        content_length,
        etag: header_string(&headers, ETAG),
        last_modified: header_string(&headers, LAST_MODIFIED),
        accepts_ranges: header_string(&headers, ACCEPT_RANGES)
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false),
    };

    Ok((bytes, meta))
}

fn header_string(headers: &reqwest::header::HeaderMap, key: reqwest::header::HeaderName) -> Option<String> {
    headers.get(key).and_then(|v| v.to_str().ok()).map(|v| v.to_string())
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
    fn apply_color_pipeline(&mut self) {
        // Intentionally no-op for milestone 1. All images still traverse this
        // stage so future filters can be inserted without changing control flow.
    }

    /// Scale and center-crop to fill the target aspect ratio.
    fn fill_crop(&mut self, target_w: u32, target_h: u32) {
        let src = self.img.to_rgba8();
        let (src_w, src_h) = src.dimensions();

        if src_w == 0 || src_h == 0 || target_w == 0 || target_h == 0 {
            self.img = DynamicImage::ImageRgba8(RgbaImage::new(target_w.max(1), target_h.max(1)));
            return;
        }

        let scale_w = target_w as f32 / src_w as f32;
        let scale_h = target_h as f32 / src_h as f32;
        let scale = scale_w.max(scale_h);

        let new_w = ((src_w as f32) * scale).ceil() as u32;
        let new_h = ((src_h as f32) * scale).ceil() as u32;

        let resized = resize(&src, new_w, new_h, FilterType::Lanczos3);
        let x = (new_w.saturating_sub(target_w)) / 2;
        let y = (new_h.saturating_sub(target_h)) / 2;

        let cropped = crop_imm(&resized, x, y, target_w, target_h).to_image();
        self.img = DynamicImage::ImageRgba8(cropped);
    }

    fn encode_jpeg(&self, quality: u8, bg: [u8; 3]) -> Result<Vec<u8>, String> {
        let rgba = self.img.to_rgba8();
        let rgb = flatten_alpha(&rgba, bg);
        let mut out = Vec::new();
        let mut enc = JpegEncoder::new_with_quality(&mut out, quality);
        enc.encode(
            &rgb,
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| format!("jpeg encode failed: {e}"))?;
        Ok(out)
    }
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
