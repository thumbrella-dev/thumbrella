//! Core synchronous processing pipeline for common still-image inputs.
//!
//! Design rule: every input image goes through this same decode -> process ->
//! encode path. Even if a source contains an embedded thumbnail, we still run
//! it through post-processing so no output pixel bypasses the pipeline.

use image::imageops::{FilterType, crop_imm, resize};
use image::{DynamicImage, ImageDecoder, ImageReader, Rgba, RgbaImage, metadata::Orientation};
use mozjpeg::{ColorSpace, Compress, Marker};
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
    let upscaled = buf.fill_crop(profile.width, profile.height);
    buf.apply_color_pipeline(upscaled);

    // Upscaled sources often have blocky low-detail content (sprites/icons).
    // A lower JPEG quality keeps output size bounded for those cases.
    let effective_quality = if upscaled { 15 } else { profile.quality };

    let thumb = buf.encode_jpeg(effective_quality, profile.background)?;
    let info = RenderInfo {
        input_width,
        input_height,
        source_orientation: orientation_name(orientation).to_string(),
        upscaled,
        output_width: profile.width,
        output_height: profile.height,
        output_quality: effective_quality,
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
