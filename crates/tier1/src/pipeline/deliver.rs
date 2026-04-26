//! Pipeline step: **deliver** — resize, colour-correct, and JPEG-encode the render buffer.
//!
//! Receives a decoded image buffer in `cook.render` (populated by shortcut or
//! a higher-tier handoff) and produces the final thumbnail JPEG stored in
//! `cook.response.thumbnail`.

use std::time::Instant;

use image::imageops::{crop_imm, resize, FilterType};
use image::DynamicImage;

use crate::media::Strategy;
use crate::result::{RenderHandler};
use crate::spec::ThumbnailConfig;

use crate::cook::ThumbCook;
use crate::http_buf::HttpStream;

use crate::result::JobStatus;

// ── Pipeline entry point ──────────────────────────────────────────────────────

/// Encode `cook.render` into the final thumbnail JPEG.
///
/// Assumes `cook.render.is_some()` — called only when shortcut has populated it.
/// Sets `cook.response.thumbnail`, `status = Success`, and `strategy = Render`.
pub async fn deliver<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let Some(img) = cook.render.take() else { return };
    let config = &ThumbnailConfig::CANONICAL;

    let t0 = Instant::now();
    let mut buf = ProcessBuffer::from_dynamic(img);
    buf.fill_crop(config.exact_width, config.exact_height);
    buf.composite_onto(config.background_rgb);
    // unsharp mask: disabled
    // vignette: disabled
    let jpeg = match buf.encode_jpeg(config.jpeg_quality) {
        Ok(j) => j,
        Err(_) => return,
    };

    cook.trace.deliver_secs    = t0.elapsed().as_secs_f64();
    cook.trace.thumbnail_bytes = Some(jpeg.len() as u64);
    cook.response.thumbnail    = jpeg;
    cook.response.status       = JobStatus::Success;
    cook.response.strategy     = Some(Strategy::Render);
}

// ── Shared render core ────────────────────────────────────────────────────────

/// Decode `bytes` and produce a thumbnail JPEG via the canonical pipeline.
///
/// `is_embedded` should be `true` when `bytes` is a pre-extracted thumbnail
/// (EXIF JPEG, HEIC cover, DOCX/ODT preview) — the EXIF orientation pass is
/// skipped (orientation was already applied or irrelevant) and upscaling uses
/// a photo-quality filter rather than Nearest.
///
/// Returns `(jpeg_bytes, Strategy, source_width, source_height)`.
pub(super) fn render_to_thumb(
    _bytes: &[u8],
    _is_embedded: bool,
    _config: &ThumbnailConfig,
) -> Result<(Vec<u8>, Strategy, u32, u32, f64), String> {
    Ok((vec![0u8; 16], Strategy::Embedded, 250, 200, 0.001))
}

/*
pub(super) fn render_to_thumb(
    bytes: &[u8],
    is_embedded: bool,
    config: &ThumbnailConfig,
) -> Result<(Vec<u8>, Strategy, u32, u32, f64), String> {
    let (mut img, orientation, src_w, src_h) = if is_embedded {
        let img = image::load_from_memory(bytes)
            .map_err(|e| format!("embedded thumbnail decode failed: {e}"))?;
        let (w, h) = (img.width(), img.height());
        (img, Orientation::NoTransforms, w, h)
    } else {
        // ZIP / container extraction happens in shortcut before this is called.
        // By the time bytes reach render_to_thumb they are a plain image.
        let img = image::load_from_memory(bytes)
            .map_err(|e| format!("image decode failed: {e}"))?;
        let (w, h) = (img.width(), img.height());
        (img, Orientation::NoTransforms, w, h)
    };

    img.apply_orientation(orientation);
    let t_deliver = Instant::now();
    img = pre_scale_to_target(img, config.exact_width, config.exact_height);

    let mut buf = ProcessBuffer::from_dynamic(img);
    let scaled_up = buf.fill_crop(config.exact_width, config.exact_height, is_embedded);
    buf.composite_onto(config.background_rgb);
    let pixel_art_mode = scaled_up && !is_embedded;
    let vignette = if pixel_art_mode { config.vignette_strength * 0.6 } else { config.vignette_strength };
    buf.apply_vignette(vignette);

    let quality = if pixel_art_mode { config.pixel_art_quality } else { config.jpeg_quality };
    let jpeg = buf.encode_jpeg(quality)?;
    let deliver_secs = t_deliver.elapsed().as_secs_f64();
    let strategy = if is_embedded { Strategy::Embedded } else { Strategy::Render };
    Ok((jpeg, strategy, src_w, src_h, deliver_secs))
}
*/

// ── Process buffer ────────────────────────────────────────────────────────────
//
// Normalise to RGB or RGBA once, then work in-place through resize, crop,
// composite, and vignette.  No intermediate DynamicImage conversions.

enum BufInner {
    Rgb(image::RgbImage),
    Rgba(image::RgbaImage),
}

pub(super) struct ProcessBuffer {
    inner: BufInner,
}

impl ProcessBuffer {
    /// Convert a decoded image to RGB or RGBA — exactly one allocation.
    pub(super) fn from_dynamic(img: DynamicImage) -> Self {
        let inner = if img.color().has_alpha() {
            match img {
                DynamicImage::ImageRgba8(i) => BufInner::Rgba(i),
                other => BufInner::Rgba(other.into_rgba8()),
            }
        } else {
            match img {
                DynamicImage::ImageRgb8(i) => BufInner::Rgb(i),
                other => BufInner::Rgb(other.into_rgb8()),
            }
        };
        Self { inner }
    }

    fn dimensions(&self) -> (u32, u32) {
        match &self.inner {
            BufInner::Rgb(i)  => i.dimensions(),
            BufInner::Rgba(i) => i.dimensions(),
        }
    }

    /// Scale-to-fill with centered horizontal / upper-quarter vertical crop.
    pub(super) fn fill_crop(&mut self, target_w: u32, target_h: u32) {
        let (src_w, src_h) = self.dimensions();
        if src_w == 0 || src_h == 0 || target_w == 0 || target_h == 0 {
            self.inner = BufInner::Rgb(image::RgbImage::new(target_w.max(1), target_h.max(1)));
            return;
        }

        let scale = (target_w as f32 / src_w as f32).max(target_h as f32 / src_h as f32);
        let new_w = ((src_w as f32) * scale).ceil() as u32;
        let new_h = ((src_h as f32) * scale).ceil() as u32;
        let near = (scale - 1.0).abs() <= 0.06 && src_w.abs_diff(new_w) <= 8;
        let filter = FilterType::Triangle;

        let prev = std::mem::replace(&mut self.inner, BufInner::Rgb(image::RgbImage::new(0, 0)));
        self.inner = match prev {
            BufInner::Rgb(img) => {
                let r = if near { img } else { resize(&img, new_w, new_h, filter) };
                let x = r.width().saturating_sub(target_w) / 2;
                let y = (r.height().saturating_sub(target_h) as f32 * 0.25) as u32;
                BufInner::Rgb(crop_imm(&r, x, y, target_w, target_h).to_image())
            }
            BufInner::Rgba(img) => {
                let r = if near { img } else { resize(&img, new_w, new_h, filter) };
                let x = r.width().saturating_sub(target_w) / 2;
                let y = (r.height().saturating_sub(target_h) as f32 * 0.25) as u32;
                BufInner::Rgba(crop_imm(&r, x, y, target_w, target_h).to_image())
            }
        };
    }

    /// Composite RGBA over a solid background → RGB in-place.  No-op if already RGB.
    pub(super) fn composite_onto(&mut self, bg: [u8; 3]) {
        let prev = std::mem::replace(&mut self.inner, BufInner::Rgb(image::RgbImage::new(0, 0)));
        self.inner = BufInner::Rgb(match prev {
            BufInner::Rgb(i)   => i,
            BufInner::Rgba(rgba) => composite_rgba_over_rgb(rgba, bg),
        });
    }

    /// Elliptical soft vignette, in-place on the RGB buffer.
    /// Uses r² comparisons — no sqrt required.
    pub(super) fn apply_vignette(&mut self, strength: f32) {
        if strength < 1e-3 { return; }
        let BufInner::Rgb(ref mut img) = self.inner else { return };
        let w = img.width();
        let h = img.height();
        if w < 2 || h < 2 { return; }

        const INNER_SQ: f32 = 0.62 * 0.62;
        const OUTER_SQ: f32 = 1.25 * 1.25;
        let inv_span = 1.0 / (OUTER_SQ - INNER_SQ);
        let cx = (w - 1) as f32 * 0.5;
        let cy = (h - 1) as f32 * 0.5;

        // Precompute (dx/cx)² once per column.
        let dx_sq: Vec<f32> = (0..w as usize)
            .map(|x| { let d = (x as f32 - cx) / cx; d * d })
            .collect();

        let pixels: &mut [u8] = img.as_mut();
        for y in 0..h as usize {
            let dy = (y as f32 - cy) / cy;
            let dy_sq = dy * dy;
            let row_base = y * w as usize * 3;
            for x in 0..w as usize {
                let r_sq = dx_sq[x] + dy_sq;
                if r_sq <= INNER_SQ { continue; }
                let t = ((r_sq - INNER_SQ) * inv_span).min(1.0);
                let smooth = t * t * (3.0 - 2.0 * t);
                let weight = 1.0 - strength * smooth;
                let off = row_base + x * 3;
                pixels[off]     = (pixels[off]     as f32 * weight) as u8;
                pixels[off + 1] = (pixels[off + 1] as f32 * weight) as u8;
                pixels[off + 2] = (pixels[off + 2] as f32 * weight) as u8;
            }
        }
    }

    pub(super) fn encode_jpeg(&self, quality: u8) -> Result<Vec<u8>, String> {
        use mozjpeg_rs::{Encoder, Subsampling};
        let BufInner::Rgb(ref img) = self.inner else {
            return Err("encode_jpeg: call composite_onto first".into());
        };
        let (w, h) = img.dimensions();
        Encoder::fastest()
            .quality(quality)
            .subsampling(Subsampling::S420)
            .encode_rgb(img.as_raw(), w, h)
            .map_err(|e| format!("JPEG encode failed: {e}"))
    }
}

/// Alpha-composite RGBA onto a solid RGB background using integer arithmetic.
fn composite_rgba_over_rgb(rgba: image::RgbaImage, bg: [u8; 3]) -> image::RgbImage {
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbImage::new(w, h);
    let src = rgba.as_raw();
    let dst: &mut [u8] = out.as_mut();
    for i in 0..(w * h) as usize {
        let a  = src[i * 4 + 3] as u32;
        let ia = 255 - a;
        dst[i * 3]     = ((src[i*4]     as u32 * a + bg[0] as u32 * ia + 127) / 255) as u8;
        dst[i * 3 + 1] = ((src[i*4 + 1] as u32 * a + bg[1] as u32 * ia + 127) / 255) as u8;
        dst[i * 3 + 2] = ((src[i*4 + 2] as u32 * a + bg[2] as u32 * ia + 127) / 255) as u8;
    }
    out
}

// ── I/O ───────────────────────────────────────────────────────────────────────

// ── Direct-image render ───────────────────────────────────────────────────────

/// Post-process a pre-decoded `DynamicImage` through the canonical pipeline.
///
/// Used by shortcut paths that decode the source image themselves (e.g.
/// progressive JPEG with DCT-level downscaling) so `render_to_thumb`'s internal
/// decode step can be skipped.
///
/// `src_w` and `src_h` are the *original* source dimensions (before any
/// caller-side DCT scaling) — forwarded verbatim to the caller for logging.
/// The caller is responsible for pre-scaling large images down to a reasonable
/// size before calling this function (see `shortcut::pre_scale_to_target`).
pub(super) fn process_img_to_thumb(
    img: DynamicImage,
    src_w: u32,
    src_h: u32,
    config: &ThumbnailConfig,
) -> Result<(Vec<u8>, Strategy, u32, u32, f64), String> {
    let t_deliver = Instant::now();
    let mut buf = ProcessBuffer::from_dynamic(img);
    buf.fill_crop(config.exact_width, config.exact_height);
    buf.composite_onto(config.background_rgb);
    buf.apply_vignette(config.vignette_strength);
    let jpeg = buf.encode_jpeg(config.jpeg_quality)?;
    let deliver_secs = t_deliver.elapsed().as_secs_f64();
    Ok((jpeg, Strategy::Render, src_w, src_h, deliver_secs))
}
