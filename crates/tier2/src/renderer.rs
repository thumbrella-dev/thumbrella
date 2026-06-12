//! Tier 2 in-process renderer.
//!
//! Implements [`InProcessRenderer`] for extended formats that tier 1 cannot
//! handle natively: HEIC/AVIF, EXR/HDR, video, SVG, and documents.
//!
//! # How the HTTP connection is used
//!
//! The renderer receives `&mut dyn RenderCook`.  Calling `cook.take_reader()`
//! enters streaming mode on the `HttpBuffer` and moves it out as a
//! `Box<dyn ReadSeek + Send>`.  This reader is passed directly into
//! `spawn_blocking` where libav's `AVIOContext` callbacks pull bytes through
//! the paged cache on-demand — no full-file drain, no clone, no second request.
//!
//! For common raster formats (JPEG, PNG, …) the image crate still needs all
//! bytes, but the bytes arrive through the same streaming reader path; they
//! are collected via `read_to_end` inside the blocking task rather than being
//! drained in the async context before spawning.

use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;

use image::{DynamicImage, imageops};
use tier1::InProcessRenderer;
use tier1::ReadSeek;
use tier1::RenderCook;
use tier1::media::FileKind;
use tier1::renderer::{RenderOutput, apply_render_output};
use tier1::spec::ThumbnailConfig;

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Tier2Renderer;

impl Tier2Renderer {
    pub fn new() -> Self { Self }
    pub fn shared() -> Arc<Self> { Arc::new(Self) }
}

impl Default for Tier2Renderer {
    fn default() -> Self { Self }
}

impl InProcessRenderer for Tier2Renderer {
    fn render<'a>(
        &'a self,
        cook: &'a mut dyn RenderCook,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let kind           = cook.media_kind();
            let ext            = cook.media_extension().unwrap_or("?").to_string();
            let content_length = cook.content_length();
            eprintln!("[tier2] render: kind={kind:?}  ext={ext}  content_length={content_length:?}");

            match kind {
                Some(FileKind::Image) => {
                    // Formats tier2's libav can't decode — return false
                    // immediately so tier3's oiiotool / ffmpeg CLI can
                    // take over without the reader being consumed.
                    if !tier2_handles_image(&ext) {
                        eprintln!("[tier2] unsupported image format {ext} — deferring to higher tier");
                        return false;
                    }
                    // Early-exit for arithmetic JPEGs: libav's mjpeg decoder
                    // does not support SOF9/SOF10.
                    if is_jpeg_format(&ext) && is_arithmetic_peek(cook) {
                        eprintln!("[tier2] arithmetic JPEG detected — deferring to higher tier");
                        return false;
                    }
                    render_image(cook, &ext, content_length).await
                }
                Some(FileKind::Video) => render_video(cook, &ext, content_length).await,
                Some(FileKind::Vector) => render_vector(cook, &ext, content_length).await,
                //       FileKind::Document → pdfium first page
                _ => false,
            }
        })
    }
}

// ── Image decode ──────────────────────────────────────────────────────────────

fn is_image_crate_format(ext: &str) -> bool {
    matches!(ext, "png")
    // image library seems to handle interlaced png more efficiently than libav.
    // but otherwise their performance is within 10%
}

/// Returns `true` for image formats that tier2's libav can decode.
/// Studio formats (EXR, HDR, DPX, …) and esoteric formats are handled by
/// tier3's oiiotool / ffmpeg CLI.
fn tier2_handles_image(ext: &str) -> bool {
    matches!(ext,
        "jpeg" | "jpg" | "png" | "webp" | "bmp" | "tiff" | "tif"
        | "gif" | "ico" | "psd" | "avif" | "heic" | "heif"
        | "dng" | "cr2" | "nef" | "arw" | "orf" | "rw2"
        | "pef" | "srw" | "raf" | "3fr" | "fff" | "iiq" | "raw"
    )
}


/// Returns `true` for TIFF-based camera raw formats.
///
/// These formats embed a JPEG preview inside a SubIFD that can be extracted
/// with a few small range requests, avoiding a full raw-sensor download.
fn is_raw_format(ext: &str) -> bool {
    matches!(
        ext,
        "dng" | "cr2" | "nef" | "arw" | "orf" | "rw2" | "pef" | "srw" | "raf"
            | "3fr" | "fff" | "iiq" | "raw"
    )
}

/// Returns `true` for JPEG XL — decoded by the pure-Rust `jxl-oxide` crate.
fn is_jxl_format(ext: &str) -> bool {
    ext == "jxl"
}

/// Returns `true` for JPEG — decoded by libav, with arithmetic-coding
/// detection so tier 3 can take over for unsupported variants.
fn is_jpeg_format(ext: &str) -> bool {
    matches!(ext, "jpeg" | "jpg")
}

/// Returns `true` for SVG — rendered by the pure-Rust `resvg` crate.
fn is_svg_format(ext: &str) -> bool {
    ext == "svg"
}

/// Peek at the already-cached first page of a JPEG to detect arithmetic
/// coding.  Uses [`RenderCook::peek_bytes`] so the reader is not consumed.
///
/// Returns `true` if an arithmetic SOF marker (0xC9 or 0xCA) is found.
fn is_arithmetic_peek(cook: &dyn RenderCook) -> bool {
    let Some(buf) = cook.peek_bytes(512) else { return false; };
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return false; // not JPEG
    }
    let mut i = 2;
    while i + 3 < buf.len() {
        if buf[i] == 0xFF {
            match buf[i + 1] {
                0x00 | 0xFF => { i += 1; continue; }
                0xC9 | 0xCA => return true,
                0xDA => break,
                _ => {
                    let seg_len = ((buf[i + 2] as usize) << 8) | (buf[i + 3] as usize);
                    i += 2 + seg_len;
                    continue;
                }
            }
        }
        i += 1;
    }
    false
}

/// Peek at the first bytes of a JPEG stream to detect arithmetic coding.
///
/// Arithmetic-coded JPEGs use SOF9 (0xC9) or SOF10 (0xCA) markers.  Libav's
/// built-in mjpeg decoder does not support these — it returns
/// "unsupported coding type (c9)".  When detected, tier 2 returns false
/// so the cook can fall back to tier 3's ffmpeg CLI.
///
/// Returns `true` if arithmetic coding is detected.
fn detect_arithmetic_jpeg(reader: &mut dyn ReadSeek) -> bool {
    use std::io::SeekFrom;

    let mut buf = [0u8; 512];
    let n = reader.read(&mut buf).unwrap_or(0);
    // Always seek back — caller continues from the start regardless.
    let _ = reader.seek(SeekFrom::Start(0));

    if n < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return false; // not JPEG
    }

    let mut i = 2; // skip SOI
    while i + 3 < n {
        if buf[i] == 0xFF {
            let marker = buf[i + 1];
            match marker {
                0x00 | 0xFF => { i += 1; continue; } // stuffed byte or padding
                0xC9 | 0xCA => return true,             // SOF9/SOF10 = arithmetic
                0xDA => break,                           // SOS — scan data follows
                _ => {
                    // Segment with a length field — skip past it.
                    let seg_len = ((buf[i + 2] as usize) << 8) | (buf[i + 3] as usize);
                    i += 2 + seg_len;
                    continue;
                }
            }
        }
        i += 1;
    }
    false
}

// ── JPEG XL decode ────────────────────────────────────────────────────────────

/// Decode a JPEG XL image via the `jxl-oxide` crate.
///
/// Expects `reader` to have all bytes available (reads via `read_to_end`).
/// Returns `Some(RenderOutput)` on success, `None` on decode error.
fn decode_jxl(reader: &mut dyn ReadSeek) -> Option<RenderOutput> {
    use jxl_oxide::JxlImage;

    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).ok()?;
    let image = JxlImage::builder().read(std::io::Cursor::new(&bytes)).ok()?;
    let (width, height) = (image.width(), image.height());

    let render = image.render_frame(0).ok()?;

    let fb = render.image_all_channels();
    let channels = fb.channels();
    let buf = fb.buf();

    let img: DynamicImage = match channels {
        1 => {
            let gray: Vec<u8> = buf.iter().map(|&v| (v.clamp(0.0, 1.0) * 255.0) as u8).collect();
            DynamicImage::ImageLuma8(image::GrayImage::from_raw(width, height, gray)?)
        }
        3 => {
            let rgb: Vec<u8> = buf.iter().map(|&v| (v.clamp(0.0, 1.0) * 255.0) as u8).collect();
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, rgb)?)
        }
        _ => {
            // 4 channels (RGBA) or more — just take first 3
            let rgb: Vec<u8> = buf.chunks(channels)
                .flat_map(|px| px.iter().take(3).map(|&v| (v.clamp(0.0, 1.0) * 255.0) as u8))
                .collect();
            DynamicImage::ImageRgb8(image::RgbImage::from_raw(width, height, rgb)?)
        }
    };

    let depth = channels as u32 * 8;
    let cfg = ThumbnailConfig::CANONICAL;
    let img = pre_scale(img, cfg.exact_width, cfg.exact_height);

    Some(RenderOutput {
        image:           img,
        renderer:        Some("jxl_oxide".into()),
        codec:           None,
        video_seek_secs: None,
        properties:      Some(serde_json::json!({ "width_pixels": width, "height_pixels": height, "bits_per_pixel": depth })),
    })
}

// ── SVG render ────────────────────────────────────────────────────────────────

/// Render an SVG to a raster image via the `resvg` crate.
///
/// We rasterise at 500×400 (2× the canonical thumbnail) with default
/// quality settings (anti-aliasing on) so the downstream deliver step
/// has enough pixel data for a clean Lanczos3 downscale to 250×200.
fn render_svg(reader: &mut dyn ReadSeek) -> Option<RenderOutput> {
    let mut svg_str = String::new();
    reader.read_to_string(&mut svg_str).ok()?;

    let tree = resvg::usvg::Tree::from_str(&svg_str, &resvg::usvg::Options::default()).ok()?;
    let svg_size = tree.size();
    let (svg_w, svg_h) = (svg_size.width() as f64, svg_size.height() as f64);
    if svg_w <= 0.0 || svg_h <= 0.0 { return None; }

    // Render at 500×400 — fit the SVG inside, preserving aspect ratio.
    const RENDER_W: u32 = 500;
    const RENDER_H: u32 = 400;
    let scale = (RENDER_W as f64 / svg_w)
        .min(RENDER_H as f64 / svg_h)
        .min(1.0);
    let rw = (svg_w * scale).ceil() as u32;
    let rh = (svg_h * scale).ceil() as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(rw, rh)?;
    let transform = resvg::usvg::Transform::from_scale(scale as f32, scale as f32);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let rgba = pixmap.take();
    let img = DynamicImage::ImageRgba8(image::RgbaImage::from_raw(rw, rh, rgba)?);
    let (src_w, src_h) = (svg_w.ceil() as u32, svg_h.ceil() as u32);

    Some(RenderOutput {
        image:           img,
        renderer:        Some("resvg".into()),
        codec:           None,
        video_seek_secs: None,
        properties:      Some(serde_json::json!({ "width_pixels": src_w, "height_pixels": src_h, "bits_per_pixel": 32 })),
    })
}

// ── Raw TIFF preview extraction ───────────────────────────────────────────────
//
// TIFF-based raw formats (DNG, CR2, NEF, …) embed a JPEG preview inside a
// SubIFD (tag 0x014A).  The SubIFD offset array lives in IFD0 (in the first
// 32 KB, already cached from inspect), but each SubIFD entry table is at a
// file offset of 100 KB–400 KB.
//
// Strategy: after taking the reader, do everything synchronously in
// spawn_blocking — seek + read to each SubIFD table, find the best JPEG
// preview strip, seek + read the JPEG bytes, then decode.  No async or trait
// plumbing needed; the ReadSeek reader issues Range requests on demand via
// block_on internally.

/// Try to extract an embedded JPEG preview from a TIFF-based raw file.
///
/// Takes the already-taken streaming reader from `render_image`.  Returns
/// `Some(RenderOutput)` on success or `None` (with the reader passed back)
/// so callers can fall through to libav.
fn extract_raw_preview(
    reader: Box<dyn ReadSeek + Send>,
) -> Result<RenderOutput, Box<dyn ReadSeek + Send>> {
    #[allow(unused_mut)] let mut reader = reader;
    use image::GenericImageView;
    use std::io::{Read, SeekFrom};

    // Read the TIFF header (first 32 KB — likely already cached).
    const HDR: usize = 32 * 1024;
    let mut header = vec![0u8; HDR];
    if reader.seek(SeekFrom::Start(0)).is_err() { return Err(reader); }
    let n = reader.read(&mut header).unwrap_or(0);
    header.truncate(n);
    if header.len() < 8 { return Err(reader); }

    let little = match &header[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(reader),
    };
    if raw_u16(&header, 2, little) != Some(42) { return Err(reader); }

    let sub_offs = raw_subifd_offsets(&header, little);
    if sub_offs.is_empty() { return Err(reader); }

    // Fetch each SubIFD entry table (~512 B each) and pick the largest
    // JPEG-compressed strip up to 10 MB.
    let mut best: Option<(u64, usize)> = None;
    let mut sub_buf = vec![0u8; 512];
    for off in sub_offs {
        if reader.seek(SeekFrom::Start(off)).is_err() { continue }
        let n = reader.read(&mut sub_buf).unwrap_or(0);
        let Some((jpeg_off, jpeg_len)) = raw_ifd_jpeg_span(&sub_buf[..n], little) else { continue };
        if jpeg_len < 4 || jpeg_len > 10_000_000 { continue }
        if best.is_none_or(|(_, bl)| jpeg_len > bl) {
            best = Some((jpeg_off, jpeg_len));
        }
    }

    let (jpeg_off, jpeg_len) = match best {
        Some(b) => b,
        None    => return Err(reader),
    };

    let mut jpeg_bytes = vec![0u8; jpeg_len];
    if reader.seek(SeekFrom::Start(jpeg_off)).is_err() { return Err(reader); }
    reader.read_exact(&mut jpeg_bytes).ok();  // may be partial on EOF
    jpeg_bytes.retain(|_| true); // keep whatever was read

    if jpeg_bytes.len() < 4 || jpeg_bytes[0] != 0xFF || jpeg_bytes[1] != 0xD8 {
        return Err(reader);
    }
    eprintln!("[tier2] raw_preview: {} bytes JPEG @ offset {}", jpeg_bytes.len(), jpeg_off);

    let mut img = match image::load_from_memory(&jpeg_bytes) {
        Ok(i) => i,
        Err(_) => return Err(reader),
    };
    let (src_w, src_h) = img.dimensions();
    let depth = img.color().bits_per_pixel();

    // The embedded JPEG in DNG files is often physically rotated 90° CW in its
    // bytes, and may also carry an EXIF orientation tag. To avoid double-rotation
    // when an EXIF tag is present, we must undo the physical rotation first.
    // A 270° CW rotation corrects a 90° CW physical rotation.
    img = DynamicImage::ImageRgba8(img.to_rgba8()).rotate270();

    // Do not apply EXIF orientation for raw-container previews.
    //
    // Many DNG/RAW files embed a JPEG preview that is already physically
    // oriented for display while still carrying a copied orientation tag
    // from the parent container. Applying EXIF here can double-rotate the
    // preview (commonly +90°). For raw previews we treat decoded pixels as
    // display-ready and skip orientation transforms.
    let cfg = ThumbnailConfig::CANONICAL;
    let img = pre_scale(img, cfg.exact_width, cfg.exact_height);

    Ok(RenderOutput {
        image:           img,
        renderer:        Some("raw_preview".into()),
        codec:           None,
        video_seek_secs: None,
        properties: Some(serde_json::json!({ "width_pixels": src_w, "height_pixels": src_h, "bits_per_pixel": depth })),
    })
}

/// Collect SubIFD (tag 0x014A) file offsets from IFD0 that lie beyond `bytes.len()`.
fn raw_subifd_offsets(bytes: &[u8], little: bool) -> Vec<u64> {
    let ifd0_off = match raw_u32(bytes, 4, little) { Some(v) => v as usize, None => return vec![] };
    if ifd0_off + 2 > bytes.len() { return vec![]; }
    let count = match raw_u16(bytes, ifd0_off, little) { Some(v) => v as usize, None => return vec![] };
    let mut result = vec![];
    for i in 0..count {
        let e = ifd0_off + 2 + i * 12;
        if e + 12 > bytes.len() { break; }
        let tag = match raw_u16(bytes, e,     little) { Some(v) => v, None => break };
        let ft  = match raw_u16(bytes, e + 2, little) { Some(v) => v, None => break };
        let fc  = match raw_u32(bytes, e + 4, little) { Some(v) => v as usize, None => break };
        let v   = match raw_u32(bytes, e + 8, little) { Some(v) => v as usize, None => break };
        if tag != 0x014A || ft != 4 { continue }
        if fc == 1 {
            if v > bytes.len() { result.push(v as u64); }
        } else {
            for j in 0..fc.min(32) {
                if let Some(off) = raw_u32(bytes, v + j * 4, little) {
                    if off as usize > bytes.len() { result.push(off as u64); }
                }
            }
        }
    }
    result
}

/// Parse a small IFD buffer for a JPEG-compressed strip.
///
/// Returns `(jpeg_file_offset, jpeg_byte_count)` when Compression ∈ {6, 7}
/// and both StripOffsets + StripByteCounts are present.
fn raw_ifd_jpeg_span(data: &[u8], little: bool) -> Option<(u64, usize)> {
    if data.len() < 2 { return None; }
    let count = raw_u16(data, 0, little)? as usize;
    let mut compression: Option<u16>   = None;
    let mut strip_off:   Option<u64>   = None;
    let mut strip_cnt:   Option<usize> = None;
    for i in 0..count {
        let e = 2 + i * 12;
        if e + 12 > data.len() { break; }
        let tag = match raw_u16(data, e, little) { Some(v) => v, None => break };
        let v   = match raw_u32(data, e + 8, little) { Some(v) => v as u64, None => break };
        match tag {
            0x0103 => compression = Some(v as u16),
            0x0111 => strip_off   = Some(v),
            0x0117 => strip_cnt   = Some(v as usize),
            _ => {}
        }
    }
    if !matches!(compression, Some(6 | 7)) { return None; }
    Some((strip_off?, strip_cnt?))
}

#[inline] fn raw_u16(b: &[u8], off: usize, little: bool) -> Option<u16> {
    let s: [u8; 2] = b.get(off..off + 2)?.try_into().ok()?;
    Some(if little { u16::from_le_bytes(s) } else { u16::from_be_bytes(s) })
}
#[inline] fn raw_u32(b: &[u8], off: usize, little: bool) -> Option<u32> {
    let s: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
    Some(if little { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
}

/// Render a still image.
async fn render_image(
    cook: &mut dyn RenderCook,
    ext: &str,
    content_length: Option<u64>,
) -> bool {
    let reader = match cook.take_reader() {
        Some(r) => r,
        None => return render_image_fallback(cook, ext, content_length).await,
    };

    let ext_owned = ext.to_string();
    eprintln!("[tier2] render_image: streaming reader  ext={ext}");

    let (result, avio_bytes): (Option<RenderOutput>, u64) = if is_image_crate_format(ext) {
        tokio::task::spawn_blocking(move || {
            (collect_and_decode_image_crate(reader, content_length, &ext_owned), 0u64)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    } else if is_raw_format(&ext_owned) {
        // Raw format: try SubIFD JPEG preview first; fall back to libav.
        tokio::task::spawn_blocking(move || {
            match extract_raw_preview(reader) {
                Ok(out) => (Some(out), 0u64),
                Err(reader) => {
                    // No extractable preview — some raw formats (e.g. CR3) use
                    // a MOV/ISOBMFF container that libav can handle.
                    let mut reader = reader;
                    let rotation_hint = container_rotation_hint(&mut *reader);
                    crate::avdecode::decode_with_libav(reader, content_length, Some(ext_owned), rotation_hint, None)
                }
            }
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    } else if is_jxl_format(&ext_owned) {
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let result = decode_jxl(&mut *reader);
            (result, 0u64)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    } else if is_svg_format(&ext_owned) {
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let result = render_svg(&mut *reader);
            (result, 0u64)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    } else if is_jpeg_format(&ext_owned) {
        // Check for arithmetic coding before committing to libav.
        // Arithmetic JPEGs (SOF9) are not supported by libav's mjpeg
        // decoder.  Return false so the cook can fall back to tier 3.
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            if detect_arithmetic_jpeg(&mut *reader) {
                eprintln!("[tier2] arithmetic JPEG detected — deferring to tier 3");
                // Reader is dropped here.  The renderer returns false,
                // which tells the cook the format was not handled.
                // The cook marks the result as Unavailable and finishes.
                // If a tier 3 handoff is configured, the cook escalates.
                return (None, 0u64);
            }
            let rotation_hint = container_rotation_hint(&mut *reader);
            crate::avdecode::decode_with_libav(reader, content_length, Some(ext_owned), rotation_hint, None)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    } else {
        // Sniff container rotation inside spawn_blocking (reader may block).
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let rotation_hint = container_rotation_hint(&mut *reader);
            crate::avdecode::decode_with_libav(reader, content_length, Some(ext_owned), rotation_hint, None)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    };

    if avio_bytes > 0 {
        cook.set_bytes_consumed(avio_bytes);
    }

    // Scene-linear formats (EXR, HDR) need a gamma curve applied before
    // they hit the deliver step, otherwise they look dark and crushed.
    let result = result.map(|mut out| {
        if is_linear_format(ext) {
            out.image = linear_to_srgb(out.image);
        }
        out
    });

    apply_result(cook, result)
}

/// Fallback: re-fetch the URL when no live connection is available.
async fn render_image_fallback(
    cook: &mut dyn RenderCook,
    ext: &str,
    _content_length: Option<u64>,
) -> bool {
    let url       = cook.input_url().to_string();
    let ext_owned = ext.to_string();
    let Some(bytes) = fetch_url(&url).await else {
        cook.fail_cook("fallback fetch failed (connection was not available)");
        return true;
    };
    let cl = Some(bytes.len() as u64);
    let (result, avio_bytes): (Option<RenderOutput>, u64) = if is_image_crate_format(ext) {
        tokio::task::spawn_blocking(move || {
            (collect_and_decode_image_crate(Box::new(Cursor::new(bytes)) as Box<dyn ReadSeek + Send>, cl, &ext_owned), 0u64)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    } else {
        let rotation_hint = isobmff_irot_rotation(&bytes);
        tokio::task::spawn_blocking(move || {
            let reader: Box<dyn ReadSeek + Send> = Box::new(Cursor::new(bytes));
            crate::avdecode::decode_with_libav(reader, cl, Some(ext_owned), rotation_hint, None)
        })
        .await
        .ok()
        .unwrap_or((None, 0))
    };
    if avio_bytes > 0 {
        cook.set_bytes_consumed(avio_bytes);
    }
    apply_result(cook, result)
}

/// Render a vector image (SVG) via resvg.
async fn render_vector(
    cook: &mut dyn RenderCook,
    ext: &str,
    _content_length: Option<u64>,
) -> bool {
    let reader = match cook.take_reader() {
        Some(r) => r,
        None => {
            cook.fail_cook("tier2: vector render requires a live connection");
            return true;
        }
    };

    let ext_owned = ext.to_string();
    eprintln!("[tier2] render_vector: streaming reader  ext={ext}");

    let result = if is_svg_format(&ext_owned) {
        tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            render_svg(&mut *reader)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };

    apply_result(cook, result)
}

/// Render a video thumbnail.
///
/// First pass behavior:
/// - seek to ~1s into the stream (`-ss 1` equivalent)
/// - decode first frame after seek
/// - scale to cover the canonical thumbnail dimensions
async fn render_video(
    cook: &mut dyn RenderCook,
    ext: &str,
    content_length: Option<u64>,
) -> bool {
    const VIDEO_SEEK_SECS: f64 = 1.0;

    let reader = match cook.take_reader() {
        Some(r) => r,
        None => {
            let url = cook.input_url().to_string();
            let ext_owned = ext.to_string();
            let Some(bytes) = fetch_url(&url).await else {
                cook.fail_cook("tier2: video fallback fetch failed");
                return true;
            };
            let cl = Some(bytes.len() as u64);
            let (result, avio_bytes) = tokio::task::spawn_blocking(move || {
                let reader: Box<dyn ReadSeek + Send> = Box::new(Cursor::new(bytes));
                crate::avdecode::decode_with_libav(reader, cl, Some(ext_owned), 0, Some(VIDEO_SEEK_SECS))
            })
            .await
            .ok()
            .unwrap_or((None, 0));
            if avio_bytes > 0 { cook.set_bytes_consumed(avio_bytes); }
            return apply_result(cook, result);
        }
    };

    let ext_owned = ext.to_string();
    eprintln!("[tier2] render_video: streaming reader  ext={ext}  seek={VIDEO_SEEK_SECS}s");

    let (result, avio_bytes) = tokio::task::spawn_blocking(move || {
        crate::avdecode::decode_with_libav(reader, content_length, Some(ext_owned), 0, Some(VIDEO_SEEK_SECS))
    })
    .await
    .ok()
    .unwrap_or((None, 0));

    if avio_bytes > 0 {
        cook.set_bytes_consumed(avio_bytes);
    }
    apply_result(cook, result)
}

/// Drain `reader` via `read_to_end`, then decode with the `image` crate.
/// Runs inside `spawn_blocking` — blocking I/O is expected.
///
/// For JPEG sources, [`decode_jpeg_dct`] is tried first: it uses the
/// `jpeg-decoder` crate's DCT-level power-of-two downscaling to avoid
/// decoding the full-resolution pixel buffer when only a small thumbnail
/// is needed.  For a 12 MP source image this typically reduces the number
/// of pixels decoded by 64× (1/8 scale) before any software resize.
fn collect_and_decode_image_crate(
    mut reader: Box<dyn ReadSeek + Send>,
    content_length: Option<u64>,
    ext: &str,
) -> Option<RenderOutput> {
    use image::GenericImageView;

    let mut bytes = Vec::with_capacity(content_length.unwrap_or(65536) as usize);
    std::io::Read::read_to_end(&mut reader, &mut bytes).ok()?;
    eprintln!("[tier2] collect_and_decode_image_crate: {} bytes  ext={}", bytes.len(), ext);

    // Read EXIF orientation before decoding — the `image` crate does not
    // apply it automatically.
    let orientation = exif_orientation(&bytes);

    let cfg = ThumbnailConfig::CANONICAL;

    // For JPEG, attempt a fast DCT-downscaled decode first.
    let (img, src_w, src_h, depth) = if matches!(ext, "jpeg" | "jpg") {
        match decode_jpeg_dct(&bytes, cfg.exact_width as u16, cfg.exact_height as u16) {
            Some(t) => t,
            None => {
                let img = image::load_from_memory(&bytes).ok()?;
                let (w, h) = img.dimensions();
                let d = img.color().bits_per_pixel() as u32;
                (img, w, h, d)
            }
        }
    } else {
        let img = image::load_from_memory(&bytes).ok()?;
        let (w, h) = img.dimensions();
        let d = img.color().bits_per_pixel() as u32;
        (img, w, h, d)
    };

    // Apply EXIF orientation correction.
    let img = apply_exif_orientation(img, orientation);
    let img = pre_scale(img, cfg.exact_width, cfg.exact_height);

    Some(RenderOutput {
        image:           img,
        renderer:        Some("image_crate".into()),
        codec:           None,
        video_seek_secs: None,
        properties: Some(serde_json::json!({ "width_pixels": src_w, "height_pixels": src_h, "bits_per_pixel": depth })),
    })
}

/// Decode a JPEG using DCT power-of-two downscaling.
///
/// Requests the smallest DCT scale factor that still produces an output
/// at least `req_w × req_h` pixels in both dimensions, then returns a
/// `DynamicImage` along with the *original* source dimensions and bit depth.
///
/// Returns `None` when the image is already close to the target size (no
/// scaling benefit) or when `jpeg-decoder` cannot parse the data, so the
/// caller can fall back to `image::load_from_memory`.
fn decode_jpeg_dct(
    bytes: &[u8],
    req_w: u16,
    req_h: u16,
) -> Option<(DynamicImage, u32, u32, u32)> {
    use jpeg_decoder::PixelFormat;
    use std::io::Cursor;

    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    dec.read_info().ok()?;
    let orig_info = dec.info()?;
    let src_w  = orig_info.width  as u32;
    let src_h  = orig_info.height as u32;
    let depth  = orig_info.pixel_format.pixel_bytes() as u32 * 8;

    // Only apply DCT scaling when the source is meaningfully larger than the
    // target — if we're already within 2× in both axes the decoder overhead
    // of scale() isn't worth it; fall back to the image crate path.
    if src_w <= req_w as u32 * 2 && src_h <= req_h as u32 * 2 {
        return None;
    }

    // scale() picks the largest power-of-two divisor such that the output is
    // still ≥ (req_w, req_h).  E.g. 4032×3024 → req 250×200 → picks 1/8
    // → output 504×378, skipping 64× the pixel work.
    let (out_w, out_h) = dec.scale(req_w, req_h).ok()?;
    let pixels = dec.decode().ok()?;

    let img = match orig_info.pixel_format {
        PixelFormat::L8 => {
            let buf = image::GrayImage::from_raw(out_w as u32, out_h as u32, pixels)?;
            DynamicImage::ImageLuma8(buf)
        }
        PixelFormat::RGB24 => {
            let buf = image::RgbImage::from_raw(out_w as u32, out_h as u32, pixels)?;
            DynamicImage::ImageRgb8(buf)
        }
        PixelFormat::CMYK32 => {
            // jpeg-decoder yields raw CMYK bytes; convert to RGB.
            let rgb: Vec<u8> = pixels
                .chunks_exact(4)
                .flat_map(|c| {
                    let (cc, mm, yy, kk) = (
                        c[0] as f32 / 255.0,
                        c[1] as f32 / 255.0,
                        c[2] as f32 / 255.0,
                        c[3] as f32 / 255.0,
                    );
                    [
                        ((1.0 - cc) * (1.0 - kk) * 255.0) as u8,
                        ((1.0 - mm) * (1.0 - kk) * 255.0) as u8,
                        ((1.0 - yy) * (1.0 - kk) * 255.0) as u8,
                    ]
                })
                .collect();
            let buf = image::RgbImage::from_raw(out_w as u32, out_h as u32, rgb)?;
            DynamicImage::ImageRgb8(buf)
        }
        // L16 or any future format: fall back to image crate.
        _ => return None,
    };

    Some((img, src_w, src_h, depth))
}

/// Write a `RenderOutput` (or failure) into the cook.  Always returns `true`.
fn apply_result(cook: &mut dyn RenderCook, result: Option<RenderOutput>) -> bool {
    eprintln!("[tier2] result: {}", if result.is_some() { "ok" } else { "decode failed" });
    match result {
        Some(out) => apply_render_output(cook, out),
        None      => cook.fail_cook("render failed: could not decode image"),
    }
    true
}

/// Fast thumbnail pre-scale that covers `target_w x target_h` (Lanczos3).
fn pre_scale(img: DynamicImage, target_w: u32, target_h: u32) -> DynamicImage {
    use image::imageops::FilterType;
    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 { return img; }
    // Use max (cover), not min (fit): fill_crop will crop the excess, so we
    // must never hand it an image smaller than the target in either dimension.
    let scale = (target_w as f64 / src_w as f64).max(target_h as f64 / src_h as f64);
    let new_w = ((src_w as f64 * scale).round() as u32).max(1);
    let new_h = ((src_h as f64 * scale).round() as u32).max(1);
    img.resize(new_w, new_h, FilterType::Lanczos3)
}

/// Fallback: fetch `url` — supports both `http(s)://` and `file://`.
async fn fetch_url(url: &str) -> Option<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return tokio::fs::read(path).await.ok();
    }
    reqwest::get(url).await.ok()?.bytes().await.ok().map(|b| b.to_vec())
}

// ── EXIF orientation ──────────────────────────────────────────────────────────

/// Extract the EXIF Orientation tag (0x0112) from a JPEG or TIFF byte slice.
/// Returns the raw tag value (1–8), or 1 (normal) if absent or unreadable.
fn exif_orientation(bytes: &[u8]) -> u8 {
    // JPEG: scan APP segments for the Exif APP1, then read IFD0 tag 0x0112.
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut pos = 2usize;
        while pos + 4 <= bytes.len() {
            if bytes[pos] != 0xFF { break; }
            let marker = bytes[pos + 1];
            if marker == 0xD9 { break; }
            let seg_len = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
            if seg_len < 2 { break; }
            let seg_end = (pos + 2 + seg_len).min(bytes.len());
            if marker == 0xE1 {
                let payload = &bytes[(pos + 4).min(seg_end)..seg_end];
                if payload.len() >= 6 && &payload[..6] == b"Exif\x00\x00" {
                    if let Some(v) = read_tiff_orientation(&payload[6..]) {
                        return v;
                    }
                }
            }
            pos = pos + 2 + seg_len;
        }
    }
    // TIFF: orientation is directly in IFD0.
    if bytes.len() >= 8 && (bytes.starts_with(b"II") || bytes.starts_with(b"MM")) {
        if let Some(v) = read_tiff_orientation(bytes) {
            return v;
        }
    }
    1
}

/// Read TIFF IFD0 tag 0x0112 (Orientation) from a raw TIFF byte slice.
fn read_tiff_orientation(tiff: &[u8]) -> Option<u8> {
    if tiff.len() < 8 { return None; }
    let little = match &tiff[0..2] { b"II" => true, b"MM" => false, _ => return None };
    let magic = if little { u16::from_le_bytes([tiff[2], tiff[3]]) }
                else      { u16::from_be_bytes([tiff[2], tiff[3]]) };
    if magic != 42 { return None; }
    let ifd0_off = read_u32_le_be(tiff, 4, little)? as usize;
    if ifd0_off + 2 > tiff.len() { return None; }
    let count = read_u16_le_be(tiff, ifd0_off, little)? as usize;
    for i in 0..count {
        let e = ifd0_off + 2 + i * 12;
        if e + 12 > tiff.len() { break; }
        let tag = match read_u16_le_be(tiff, e, little) { Some(v) => v, None => break };
        if tag == 0x0112 {
            // SHORT value is stored in the 4-byte value/offset field.
            let v = read_u16_le_be(tiff, e + 8, little).unwrap_or(1) as u8;
            return Some(v.clamp(1, 8));
        }
    }
    None
}

fn read_u16_le_be(data: &[u8], off: usize, little: bool) -> Option<u16> {
    if off + 2 > data.len() { return None; }
    Some(if little { u16::from_le_bytes([data[off], data[off+1]]) }
         else      { u16::from_be_bytes([data[off], data[off+1]]) })
}

// ── Linear → sRGB gamma correction ───────────────────────────────────────────

/// Scene-linear formats (EXR, HDR, Radiance RGBe, …) store light values
/// proportionally.  Without tone-mapping they render dark and crushed on an
/// sRGB display.  Apply a simple gamma-2.2 curve so the deliver step works
/// with perceptually-correct pixel data.
///
/// This is intentionally a fast approximation — not a full colour pipeline.
/// Targeted at thumbnail-quality output where the primary goal is legibility.
fn linear_to_srgb(img: image::DynamicImage) -> image::DynamicImage {
    /// sRGB transfer function (approximated as pure gamma 2.2).
    const GAMMA: f64 = 1.0 / 2.2;

    fn gamma_correct(p: &mut image::Rgb<u8>) {
        for c in &mut p.0 {
            let linear = *c as f64 / 255.0;
            let srgb   = linear.powf(GAMMA);
            *c = (srgb * 255.0).round() as u8;
        }
    }

    fn gamma_correct_rgba(p: &mut image::Rgba<u8>) {
        for c in 0..3 {
            let linear = p.0[c] as f64 / 255.0;
            let srgb   = linear.powf(GAMMA);
            p.0[c] = (srgb * 255.0).round() as u8;
        }
        // Alpha is not colour — leave it alone.
    }

    match img {
        image::DynamicImage::ImageRgb8(mut buf) => {
            buf.pixels_mut().for_each(gamma_correct);
            image::DynamicImage::ImageRgb8(buf)
        }
        image::DynamicImage::ImageRgba8(mut buf) => {
            buf.pixels_mut().for_each(gamma_correct_rgba);
            image::DynamicImage::ImageRgba8(buf)
        }
        other => other,
    }
}

/// Returns `true` for extensions that are scene-linear (needs gamma correction).
fn is_linear_format(ext: &str) -> bool {
    matches!(ext, "exr" | "hdr" | "rgbe" | "sxr" | "mxr")
}

fn read_u32_le_be(data: &[u8], off: usize, little: bool) -> Option<u32> {
    if off + 4 > data.len() { return None; }
    Some(if little { u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]) }
         else      { u32::from_be_bytes([data[off], data[off+1], data[off+2], data[off+3]]) })
}

/// Apply an EXIF orientation value (1–8) to a decoded image.
///
/// EXIF orientations:
///  1 = normal (no-op); 2 = flip H; 3 = rotate 180; 4 = flip V;
///  5 = transpose (rotate 90 CW + flip H); 6 = rotate 90 CW;
///  7 = transverse (rotate 270 CW + flip H); 8 = rotate 270 CW
fn apply_exif_orientation(img: DynamicImage, orientation: u8) -> DynamicImage {
    match orientation {
        2 => DynamicImage::ImageRgb8(imageops::flip_horizontal(&img.into_rgb8())),
        3 => img.rotate180(),
        4 => DynamicImage::ImageRgb8(imageops::flip_vertical(&img.into_rgb8())),
        5 => {
            let r = img.rotate90();
            DynamicImage::ImageRgb8(imageops::flip_horizontal(&r.into_rgb8()))
        }
        6 => img.rotate90(),
        7 => {
            let r = img.rotate270();
            DynamicImage::ImageRgb8(imageops::flip_horizontal(&r.into_rgb8()))
        }
        8 => img.rotate270(),
        _ => img,
    }
}

// ── ISOBMFF / HEIC container rotation ────────────────────────────────────────────────────

/// Read the ISOBMFF `irot` box from a seekable reader and return the
/// equivalent clockwise rotation in degrees (0 / 90 / 180 / 270).
/// Seeks back to position 0 afterwards so the caller can reuse the reader.
fn container_rotation_hint(reader: &mut dyn tier1::ReadSeek) -> i32 {
    use std::io::SeekFrom;
    let mut header = [0u8; 65536];
    let n = reader.read(&mut header).unwrap_or(0);
    let _ = reader.seek(SeekFrom::Start(0));
    isobmff_irot_rotation(&header[..n])
}

/// Scan a byte slice for the ISOBMFF `irot` box and return the clockwise
/// rotation in degrees that it encodes.
///
/// The `irot` box payload is a single byte whose two LSBs encode:
///   0 = 0 deg, 1 = 90 deg CCW (= 270 deg CW), 2 = 180 deg, 3 = 270 deg CCW (= 90 deg CW)
fn isobmff_irot_rotation(data: &[u8]) -> i32 {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
        let fcc  = &data[pos+4..pos+8];
        if fcc == b"irot" && pos + 9 <= data.len() {
            return match data[pos + 8] & 0x03 {
                1 => 270,
                2 => 180,
                3 => 90,
                _ => 0,
            };
        }
        // Recurse into container boxes that may contain irot.
        // meta and iinf are FullBoxes with a 4-byte version/flags after the header.
        let is_full_box = matches!(fcc, b"meta" | b"iinf");
        let hdr = if is_full_box { 12 } else { 8 };
        if matches!(fcc, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" |
                         b"meta" | b"iinf" | b"ipco" | b"iprp" | b"dinf") {
            let inner_start = pos + hdr;
            let inner_end = if size >= hdr { (pos + size).min(data.len()) } else { data.len() };
            if inner_start < inner_end {
                let r = isobmff_irot_rotation(&data[inner_start..inner_end]);
                if r != 0 { return r; }
            }
        }
        if size < 8 { break; }
        pos += size;
    }
    0
}
