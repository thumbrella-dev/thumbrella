//! Pipeline step: **deliver** — resize, colour-correct, and JPEG-encode the render buffer.
//!
//! Receives a decoded image buffer in `cook.render` (populated by shortcut or
//! a higher-tier handoff) and produces the final thumbnail JPEG stored in
//! `cook.response.thumbnail`.

use std::time::Instant;

use image::imageops::{crop_imm, resize, FilterType};
use image::DynamicImage;

use crate::cook::{CookStatus, ThumbCook};
use crate::http_buf::HttpStream;
use crate::media::Strategy;
use crate::spec::ThumbnailConfig;

// ── Pipeline entry point ──────────────────────────────────────────────────────

/// Encode `cook.render_image` into the final thumbnail JPEG.
///
/// Assumes `cook.render_image.is_some()` — called only when shortcut has populated it.
/// Sets `cook.out_thumbnail`, `status = Complete`, and `out_strategy = Render`.
pub async fn deliver<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let Some(img) = cook.render_image.take() else { return };
    let config = &ThumbnailConfig::CANONICAL;

    let t0 = Instant::now();
    let mut buf = ProcessBuffer::from_dynamic(img);
    buf.fill_crop(config.exact_width, config.exact_height);
    buf.composite_onto(config.background_rgb);
    let jpeg = match buf.encode_jpeg(config.jpeg_quality) {
        Ok(j) => j,
        Err(_) => return,
    };
    drop(buf);

    let deliver_secs    = t0.elapsed().as_secs_f64();
    let thumbnail_bytes = jpeg.len() as u64;
    cook.out_thumbnail        = jpeg;
    cook.out_strategy         = Some(Strategy::Render);
    cook.tel_deliver_secs     = deliver_secs;
    cook.tel_thumbnail_bytes  = Some(thumbnail_bytes);
    cook.status               = CookStatus::Complete;
}

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

        // ── AR guard + overscale pre-crop ────────────────────────────────────────
        // Pre-crop to target AR, divided by the overscale factor (1.10×), so
        // the subsequent resize to exactly target_w×target_h produces the same
        // fill-and-trim effect as the prototype's overscale-resize-then-crop —
        // but without a post-resize crop step.
        //
        // Why the overscale factor cancels out of the AR:
        //   (target_w * 1.10) / (target_h * 1.10) == target_w / target_h
        // So both the AR constraint and the positional bias (center-x, 25%-y)
        // can be applied directly to the tighter clip; scale from clip → target
        // equals ≈ target_w/clip_w ≈ 1:1 and the final crop is a ≤1-pixel
        // rounding no-op.
        //
        // DoS protection: even a 4000×2 source is clipped to a ~3×2 region
        // before resize, bounding the intermediate to ≈target_w×target_h.
        const OVERSCALE: f32 = 1.10;
        let target_ar = target_w as f32 / target_h as f32;
        let source_ar = src_w as f32 / src_h as f32;
        let (clip_w, clip_h) = if source_ar > target_ar {
            // Wide source: height is the constraining dimension.
            let w = ((src_h as f32 * target_ar) / OVERSCALE).ceil() as u32;
            let h = (src_h as f32 / OVERSCALE).ceil() as u32;
            (w.min(src_w), h.min(src_h))
        } else {
            // Tall source (or exact AR): width is the constraining dimension.
            let w = (src_w as f32 / OVERSCALE).ceil() as u32;
            let h = (src_w as f32 / target_ar / OVERSCALE).ceil() as u32;
            (w.min(src_w), h.min(src_h))
        };
        let clip_x = (src_w - clip_w) / 2;
        let clip_y = ((src_h - clip_h) as f32 * 0.25) as u32;

        let scale = (target_w as f32 / clip_w as f32).max(target_h as f32 / clip_h as f32);
        let new_w = (clip_w as f32 * scale).ceil() as u32;
        let new_h = (clip_h as f32 * scale).ceil() as u32;
        let near = (scale - 1.0).abs() <= 0.06 && clip_w.abs_diff(new_w) <= 8;
        let filter = FilterType::Triangle;

        let prev = std::mem::replace(&mut self.inner, BufInner::Rgb(image::RgbImage::new(0, 0)));
        self.inner = match prev {
            BufInner::Rgb(img) => {
                let img = if clip_x > 0 || clip_y > 0 {
                    crop_imm(&img, clip_x, clip_y, clip_w, clip_h).to_image()
                } else { img };
                let r = if near { img } else { resize(&img, new_w, new_h, filter) };
                let x = r.width().saturating_sub(target_w) / 2;
                let y = (r.height().saturating_sub(target_h) as f32 * 0.25) as u32;
                BufInner::Rgb(crop_imm(&r, x, y, target_w, target_h).to_image())
            }
            BufInner::Rgba(img) => {
                let img = if clip_x > 0 || clip_y > 0 {
                    crop_imm(&img, clip_x, clip_y, clip_w, clip_h).to_image()
                } else { img };
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


