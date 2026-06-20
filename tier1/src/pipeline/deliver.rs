//! Pipeline step: **deliver** — resize, colour-correct, and JPEG-encode the render buffer.
//!
//! Receives a decoded image buffer in `cook.render` (populated by shortcut or
//! a higher-tier handoff) and produces the final thumbnail JPEG stored in
//! `cook.response.thumbnail`.

use web_time::Instant;

use image::imageops::{crop_imm, resize, unsharpen, FilterType};
use image::DynamicImage;

use crate::cook::{CookStatus, ThumbCook};
use crate::http_buf::HttpStream;
use crate::spec::ThumbnailConfig;

// ── Pipeline entry point ──────────────────────────────────────────────────────

/// Encode `cook.render_image` into the final thumbnail JPEG.
///
/// Assumes `cook.render_image.is_some()` — called only when shortcut has populated it.
/// Sets `cook.out_thumbnail` and `status = Complete`.
pub async fn deliver<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let Some(img) = cook.render_image.take() else { return };
    cook.render_resolution = Some([img.width(), img.height()]);
    let config = &ThumbnailConfig::CANONICAL;

    // Pixel-art mode: source is genuinely tiny (sprites/icons, ≤ pixel_art_max_px
    // in *both* dimensions).  Uses nearest-neighbour resize and a lower JPEG
    // quality to preserve hard edges without ringing artifacts.
    // Embedded camera thumbnails (e.g. 192×144 EXIF previews) are photographic
    // content and must NOT trigger this path.
    // Also, progressive JPEG partial decodes produce artificially small images
    // and must NOT trigger this path — use the photographic quality instead.
    let pixel_art = !cook.render_is_progressive_partial
        && img.width() <= config.pixel_art_max_px
        && img.height() <= config.pixel_art_max_px;

    let t0 = Instant::now();
    let mut buf = ProcessBuffer::from_dynamic(img);
    buf.fit_to_target(config.exact_width, config.exact_height, config.min_fill_ratio, config.fill_budget, pixel_art);
    buf.place_on_canvas(
        config.exact_width,
        config.exact_height,
        config.background_rgb,
        cook.runtime.background_image.as_ref(),
    );
    // Post-composite processing (always on the final RGB buffer).
    if !pixel_art {
        buf.apply_unsharp_mask(0.8, 5);
    }
    buf.apply_vignette(config.vignette_strength);
    let quality = if pixel_art { config.pixel_art_quality } else { config.jpeg_quality };
    let mut jpeg = match buf.encode_jpeg(quality) {
        Ok(j) => j,
        Err(_) => return,
    };
    // If the thumbnail is unexpectedly large, re-encode at reduced quality.
    // Some images (high-frequency textures, noise) produce pathological JPEGs
    // even at small pixel dimensions.  A single retry at ~60% quality brings
    // them back under budget without visible degradation at thumbnail size.
    const SIZE_CAP: usize = 8192;
    if jpeg.len() > SIZE_CAP && quality > 10 {
        let fallback_quality = ((quality as f32) * 0.4) as u8;
        if let Ok(smaller) = buf.encode_jpeg(fallback_quality.max(10)) {
            jpeg = smaller;
        }
    }
    let jpeg = inject_exif_comment(&jpeg);
    drop(buf);

    let deliver_secs    = t0.elapsed().as_secs_f64();
    let thumbnail_bytes = jpeg.len() as u64;
    cook.out_thumbnail        = jpeg;
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

    /// Scale source to fit within `target_w × target_h` while ensuring neither
    /// output dimension falls below `min_ratio × target_dimension`.
    ///
    /// Normal ARs: pure fit-within (one dim = target, other ≤ target).
    /// Extreme wide: scale to height = min_h, center-crop width → target_w.
    /// Extreme tall: scale to width  = min_w, upper-crop  height → target_h.
    ///
    /// Call [`ProcessBuffer::place_on_canvas`] afterwards to composite the
    /// result onto the full background canvas.
    pub(super) fn fit_to_target(
        &mut self,
        target_w: u32,
        target_h: u32,
        min_ratio: f32,
        fill_budget: f32,
        pixel_art: bool,
    ) {
        let (src_w, src_h) = self.dimensions();
        if src_w == 0 || src_h == 0 || target_w == 0 || target_h == 0 {
            self.inner = BufInner::Rgb(image::RgbImage::new(target_w.max(1), target_h.max(1)));
            return;
        }

        let min_w = ((target_w as f32 * min_ratio).round() as u32).max(1);
        let min_h = ((target_h as f32 * min_ratio).round() as u32).max(1);

        // Fit-within scale: preserves source AR, fits inside the target box.
        // One dimension will equal target_*, the other will be <= target_*.
        let fit_scale = (target_w as f32 / src_w as f32).min(target_h as f32 / src_h as f32);
        let fit_w = ((src_w as f32 * fit_scale).round() as u32).clamp(1, target_w);
        let fit_h = ((src_h as f32 * fit_scale).round() as u32).clamp(1, target_h);

        // Determine (resize_w x resize_h) and the post-resize crop target.
        // Clamped cases: source AR is outside the acceptable min-fill range.
        // We scale up to the minimum acceptable size and crop the overflowing
        // dimension to the canvas maximum.
        let (resize_w, resize_h, crop_w, crop_h) = if fit_h < min_h {
            // Extremely wide: height would fall below minimum after fit-within.
            // Scale so height = min_h; width overflows target_w -> center-crop.
            let s  = min_h as f32 / src_h as f32;
            let rw = ((src_w as f32 * s).round() as u32).max(target_w);
            (rw, min_h, target_w, min_h)
        } else if fit_w < min_w {
            // Extremely tall: width would fall below minimum after fit-within.
            // Scale so width = min_w; height overflows target_h -> upper-crop.
            let s  = min_w as f32 / src_w as f32;
            let rh = ((src_h as f32 * s).round() as u32).max(target_h);
            (min_w, rh, min_w, target_h)
        } else {
            // Normal fit-within range (not clamped by min_fill_ratio).
            //
            // Sources with AR between 1:1 (1.0) and ~4:3 (1.3) snap directly to
            // full fill — they are cropped to the canvas AR with no letterbox.
            // 4:3 already snapped via the fill_budget gap check; this extends that
            // behaviour down to square sources.
            //
            // Wider sources (AR > 1.3, e.g. 7:5, 3:2, 16:9) use the fill budget:
            // scale up by at most 1/(1-fill_budget) × fit_scale, capped at
            // fill_scale.  Near-fill sources whose gap < fill_budget also snap to
            // full fill automatically; larger mismatches get a proportional blend.
            let src_ar     = src_w as f32 / src_h as f32;
            let fill_scale = (target_w as f32 / src_w as f32).max(target_h as f32 / src_h as f32);
            let max_scale  = fit_scale / (1.0 - fill_budget).max(f32::EPSILON);
            let blend = if src_ar >= 1.0 && src_ar <= 1.3 {
                fill_scale  // near-square (1:1 – ~4:3): snap directly to full fill
            } else {
                fill_scale.min(max_scale)
            };
            let rw = ((src_w as f32 * blend).round() as u32).max(1);
            let rh = ((src_h as f32 * blend).round() as u32).max(1);
            (rw, rh, rw.min(target_w), rh.min(target_h))
        };

        let needs_resize = resize_w != src_w || resize_h != src_h;
        let needs_crop   = crop_w != resize_w || crop_h != resize_h;

        // Skip resize when scale is trivially close to 1, no crop is needed, AND
        // the source already fits within the target bounds (upscaling / identity
        // only).  If the source is even one pixel larger than the resize target we
        // must resize so the content never exceeds the canvas in place_on_canvas.
        let trivial = !pixel_art
            && !needs_crop
            && src_w <= resize_w && src_h <= resize_h
            && (resize_w as f32 / src_w as f32 - 1.0).abs() <= 0.06
            && src_w.abs_diff(resize_w) <= 8;

        // Center-x, upper-quarter-y crop offset.
        let crop_x = resize_w.saturating_sub(crop_w) / 2;
        let crop_y = ((resize_h.saturating_sub(crop_h)) as f32 * 0.25) as u32;

        let filter = if pixel_art { FilterType::Nearest } else { FilterType::Triangle };

        let prev = std::mem::replace(&mut self.inner, BufInner::Rgb(image::RgbImage::new(0, 0)));
        self.inner = match prev {
            BufInner::Rgb(img) => {
                let r = if trivial || !needs_resize {
                    img
                } else {
                    resize(&img, resize_w, resize_h, filter)
                };
                let r = if needs_crop {
                    crop_imm(&r, crop_x, crop_y, crop_w, crop_h).to_image()
                } else {
                    r
                };
                BufInner::Rgb(r)
            }
            BufInner::Rgba(img) => {
                let r = if trivial || !needs_resize {
                    img
                } else if pixel_art {
                    // Nearest-neighbour: no blending, premultiply not needed.
                    resize(&img, resize_w, resize_h, FilterType::Nearest)
                } else {
                    // Triangle filter blends adjacent pixels.  Premultiplied-alpha space
                    // prevents dark halos at transparent region boundaries.
                    let mut pm = img;
                    premultiply_rgba(&mut pm);
                    let mut r = resize(&pm, resize_w, resize_h, FilterType::Triangle);
                    unpremultiply_rgba(&mut r);
                    r
                };
                let r = if needs_crop {
                    crop_imm(&r, crop_x, crop_y, crop_w, crop_h).to_image()
                } else {
                    r
                };
                BufInner::Rgba(r)
            }
        };
    }

    /// Composite content onto a `canvas_w × canvas_h` canvas and convert to RGB.
    ///
    /// Content is centred on the canvas.  When content already fills the canvas
    /// (no letterbox/pillarbox) and is RGB, the call is a zero-copy no-op.
    ///
    /// Background priority: `bg_image` when provided and canvas-sized, else solid `bg`.
    pub(super) fn place_on_canvas(
        &mut self,
        canvas_w: u32,
        canvas_h: u32,
        bg: [u8; 3],
        bg_image: Option<&image::RgbImage>,
    ) {
        let (content_w, content_h) = self.dimensions();
        let ox = (canvas_w.saturating_sub(content_w)) / 2;
        let oy = (canvas_h.saturating_sub(content_h)) / 2;

        let prev = std::mem::replace(&mut self.inner, BufInner::Rgb(image::RgbImage::new(0, 0)));
        self.inner = BufInner::Rgb(match prev {
            BufInner::Rgb(src) if content_w == canvas_w && content_h == canvas_h => {
                // Content fills canvas exactly — no background needed.
                src
            }
            BufInner::Rgb(src) => {
                let mut canvas = make_canvas(canvas_w, canvas_h, bg, bg_image);
                blit_rgb_onto(&src, &mut canvas, ox, oy);
                canvas
            }
            BufInner::Rgba(rgba) => {
                let mut canvas = make_canvas(canvas_w, canvas_h, bg, bg_image);
                composite_rgba_onto(&rgba, &mut canvas, ox, oy);
                canvas
            }
        });
    }

    /// Unsharp mask on the final RGB buffer (call after compositing).
    pub(super) fn apply_unsharp_mask(&mut self, sigma: f32, threshold: i32) {
        let BufInner::Rgb(ref mut img) = self.inner else { return };
        *img = unsharpen(img as &image::RgbImage, sigma, threshold);
    }

    /// Radial vignette using squared distance from centre — no sqrt, JPEG-friendly.
    /// `strength` = 0.0 (none) … 1.0 (corners fully black).
    pub(super) fn apply_vignette(&mut self, strength: f32) {
        if strength <= 0.0 { return; }
        let BufInner::Rgb(ref mut img) = self.inner else { return };
        let (w, h) = img.dimensions();
        let cx = w as f32 * 0.5;
        let cy = h as f32 * 0.5;
        let max_sq = cx * cx + cy * cy;
        let raw: &mut [u8] = img.as_mut();
        for y in 0..h {
            let dy = y as f32 + 0.5 - cy;
            let dy2 = dy * dy;
            for x in 0..w {
                let dx = x as f32 + 0.5 - cx;
                let sq_norm = (dx * dx + dy2) / max_sq;  // 0 at centre, 1 at corner
                let factor = 1.0 - strength * sq_norm;
                let i = (y * w + x) as usize * 3;
                raw[i]     = (raw[i]     as f32 * factor) as u8;
                raw[i + 1] = (raw[i + 1] as f32 * factor) as u8;
                raw[i + 2] = (raw[i + 2] as f32 * factor) as u8;
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
            .smoothing(5)
            .subsampling(Subsampling::S420)
            .encode_rgb(img.as_raw(), w, h)
            .map_err(|e| format!("JPEG encode failed: {e}"))
    }
}

/// Convert straight-alpha RGBA to premultiplied-alpha in-place.
fn premultiply_rgba(img: &mut image::RgbaImage) {
    for pixel in img.pixels_mut() {
        let a = pixel[3] as u32;
        pixel[0] = ((pixel[0] as u32 * a + 127) / 255) as u8;
        pixel[1] = ((pixel[1] as u32 * a + 127) / 255) as u8;
        pixel[2] = ((pixel[2] as u32 * a + 127) / 255) as u8;
    }
}

// ── EXIF comment injection ────────────────────────────────────────────────────

/// Splice an EXIF APP1 segment containing metadata (`Software`,
/// resolution, orientation) into the JPEG stream immediately after the
/// SOI marker (`FFD8`).
///
/// Mozjpeg-rs does not expose `jpeg_write_marker`, so we must splice
/// post-encode.  This is safe because JPEG parsing is marker-relative — no
/// internal offset depends on absolute byte positions.
pub fn inject_exif_comment(jpeg: &[u8]) -> Vec<u8> {
    use exif::experimental::Writer;
    use exif::{Field, In, Rational, Tag, Value};

    // ── Build EXIF TIFF blob ──────────────────────────────────────────────
    let fields = [
        Field {
            tag: Tag::Software,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![b"thumbrella.dev".to_vec()]),
        },
        Field {
            tag: Tag::XResolution,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num: 72, denom: 1 }]),
        },
        Field {
            tag: Tag::YResolution,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num: 72, denom: 1 }]),
        },
        Field {
            tag: Tag::ResolutionUnit,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![2]), // inches
        },
    ];
    let mut writer = Writer::new();
    for f in &fields {
        writer.push_field(f);
    }
    let mut tiff = std::io::Cursor::new(Vec::new());
    if writer.write(&mut tiff, false).is_err() {
        return jpeg.to_vec();
    }
    let tiff = tiff.into_inner();

    // ── Prepend APP1 wrapper ──────────────────────────────────────────────
    // APP1 structure:  FF E1  len16  "Exif\0\0"  [TIFF]
    // len includes itself (2) + "Exif\0\0" (6) + TIFF bytes.
    let app1_len = 2u16 + 6 + tiff.len() as u16;
    let mut app1 = Vec::with_capacity(2 + 2 + app1_len as usize);
    app1.push(0xFF);
    app1.push(0xE1);
    app1.extend_from_slice(&app1_len.to_be_bytes());
    app1.extend_from_slice(b"Exif\x00\x00");
    app1.extend_from_slice(&tiff);

    // ── Splice after SOI ──────────────────────────────────────────────────
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return jpeg.to_vec();
    }

    let mut out = Vec::with_capacity(jpeg.len() + app1.len());
    out.extend_from_slice(&jpeg[..2]); // SOI
    out.extend_from_slice(&app1);      // APP1 EXIF
    out.extend_from_slice(&jpeg[2..]); // rest of JPEG (JFIF, tables, data, …)
    out
}

/// Convert premultiplied-alpha RGBA back to straight-alpha in-place.
fn unpremultiply_rgba(img: &mut image::RgbaImage) {
    for pixel in img.pixels_mut() {
        let a = pixel[3] as u32;
        if a > 0 {
            pixel[0] = ((pixel[0] as u32 * 255 + a / 2) / a).min(255) as u8;
            pixel[1] = ((pixel[1] as u32 * 255 + a / 2) / a).min(255) as u8;
            pixel[2] = ((pixel[2] as u32 * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

/// Build a `w × h` RGB canvas: clone `bg_image` when canvas-sized, else fill with solid `bg`.
fn make_canvas(w: u32, h: u32, bg: [u8; 3], bg_image: Option<&image::RgbImage>) -> image::RgbImage {
    match bg_image {
        Some(img) if img.dimensions() == (w, h) => img.clone(),
        _ => {
            let mut canvas = image::RgbImage::new(w, h);
            for p in canvas.pixels_mut() { *p = image::Rgb(bg); }
            canvas
        }
    }
}

/// Row-by-row blit of an RGB image onto a canvas at offset (ox, oy).
fn blit_rgb_onto(src: &image::RgbImage, dst: &mut image::RgbImage, ox: u32, oy: u32) {
    let (sw, sh) = src.dimensions();
    let dw = dst.width();
    let src_raw = src.as_raw();
    let dst_raw: &mut [u8] = dst.as_mut();
    for y in 0..sh {
        let src_off = (y * sw) as usize * 3;
        let dst_off = ((oy + y) * dw + ox) as usize * 3;
        dst_raw[dst_off..dst_off + sw as usize * 3]
            .copy_from_slice(&src_raw[src_off..src_off + sw as usize * 3]);
    }
}

/// Alpha-composite an RGBA image onto an RGB canvas at offset (ox, oy).
fn composite_rgba_onto(rgba: &image::RgbaImage, dst: &mut image::RgbImage, ox: u32, oy: u32) {
    let (sw, sh) = rgba.dimensions();
    let dw = dst.width();
    let src = rgba.as_raw();
    let dst_raw: &mut [u8] = dst.as_mut();
    for y in 0..sh {
        for x in 0..sw {
            let si = (y * sw + x) as usize;
            let di = ((oy + y) * dw + (ox + x)) as usize;
            let a  = src[si * 4 + 3] as u32;
            let ia = 255 - a;
            dst_raw[di * 3]     = ((src[si*4]     as u32 * a + dst_raw[di*3]     as u32 * ia + 127) / 255) as u8;
            dst_raw[di * 3 + 1] = ((src[si*4 + 1] as u32 * a + dst_raw[di*3 + 1] as u32 * ia + 127) / 255) as u8;
            dst_raw[di * 3 + 2] = ((src[si*4 + 2] as u32 * a + dst_raw[di*3 + 2] as u32 * ia + 127) / 255) as u8;
        }
    }
}
