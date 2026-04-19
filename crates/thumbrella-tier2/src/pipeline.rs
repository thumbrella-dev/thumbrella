//! Tier 2 source pipeline.
//!
//! This layer is where Tier 2-specific source identification and decode paths
//! live. Unknown formats fall back to Tier 1 so Tier 2 remains a superset.

use ffmpeg_next as ffmpeg;
use image::{DynamicImage, ImageBuffer, Rgb};
use std::io::Write;
use tempfile::NamedTempFile;
use thumbrella_tier1::{ItemRequest, ItemResult, SourceMetadata, SourceRef, ThumbnailProfile};

const TIER2_LIBAV_STILL_STRATEGY: &str = "tier2_libav_still";
const TIER2_LIBAV_VIDEO_STRATEGY: &str = "tier2_libav_video";
const TIER2_EMBEDDED_HEIC_THUMBNAIL_STRATEGY: &str = "tier2_embedded_heic_thumbnail";

pub type RenderInfo = thumbrella_tier1::pipeline::RenderInfo;

/// Build source metadata for a local byte source.
pub fn metadata_from_local_bytes(bytes: &[u8], content_length: Option<u64>, last_modified: Option<String>) -> SourceMetadata {
    thumbrella_tier1::pipeline::metadata_from_local_bytes(bytes, content_length, last_modified)
}

/// Render a thumbnail from source bytes.
///
/// Tier 2-specific render loaders will be inserted ahead of the Tier 1 path.
pub fn render_thumbnail_from_bytes(bytes: &[u8], profile: &ThumbnailProfile) -> Result<(Vec<u8>, RenderInfo), String> {
    if let Some(result) = try_render_tier2(bytes, profile) {
        return result;
    }

    thumbrella_tier1::pipeline::render_thumbnail_from_bytes(bytes, profile)
}

/// Process one item with Tier 2 handlers first and Tier 1 fallback.
pub async fn process_item(item: &ItemRequest, profile: &ThumbnailProfile) -> ItemResult {
    if let Some(result) = try_process_item_tier2(item, profile).await {
        return result;
    }

    thumbrella_tier1::pipeline::process_item(item, profile).await
}

fn try_render_tier2(bytes: &[u8], profile: &ThumbnailProfile) -> Option<Result<(Vec<u8>, RenderInfo), String>> {
    // TIFF/DNG RAWs often carry embedded JPEG previews that Tier 1 can extract
    // cheaply and more reliably than full RAW decode paths.
    if looks_like_tiff_container(bytes) || looks_like_png(bytes) {
        return None;
    }

    let explicitly_tier2 = is_tier2_still_format(bytes);
    if !explicitly_tier2 && !libav_has_video_stream(bytes) {
        return None;
    }

    let is_heic = looks_like_heic(bytes);
    let prefer_smallest_stream = is_heic;
    let heic_rotation_quadrants = if is_heic {
        parse_heic_irot_quadrants(bytes).unwrap_or(0)
    } else {
        0
    };
    let decode_strategy = if is_heic {
        TIER2_EMBEDDED_HEIC_THUMBNAIL_STRATEGY
    } else if looks_like_video(bytes) {
        TIER2_LIBAV_VIDEO_STRATEGY
    } else {
        TIER2_LIBAV_STILL_STRATEGY
    };
    let decode_target = Some((profile.width, profile.height));

    let render = decode_with_libav(bytes, prefer_smallest_stream, decode_target).and_then(|decoded| {
        let img = apply_heic_rotation(decoded.image, heic_rotation_quadrants);
        thumbrella_tier1::pipeline::render_thumbnail_from_dynamic_image_with_source_dimensions(
            img,
            profile,
            bytes.len() as u64,
            decode_strategy,
            decoded.source_width,
            decoded.source_height,
        )
    });

    match render {
        Ok(result) => Some(Ok(result)),
        Err(err) if explicitly_tier2 => Some(Err(err)),
        Err(_) => None,
    }
}

async fn try_process_item_tier2(item: &ItemRequest, _profile: &ThumbnailProfile) -> Option<ItemResult> {
    // Placeholder for Tier 2 remote/source handlers. This keeps an explicit
    // hook where Tier 2 can intercept supported source families first.
    let _url = source_url(&item.source)?;
    None
}

fn source_url(source: &SourceRef) -> Option<&str> {
    match source {
        SourceRef::Url { url } => Some(url.as_str()),
    }
}

fn is_tier2_still_format(bytes: &[u8]) -> bool {
    let Some(kind) = infer::get(bytes) else {
        return looks_like_heic(bytes) || looks_like_exr(bytes);
    };

    let mime = kind.mime_type();
    matches!(mime, "image/avif" | "image/heic" | "image/heif" | "image/x-exr")
        || looks_like_heic(bytes)
        || looks_like_exr(bytes)
}

fn looks_like_heic(bytes: &[u8]) -> bool {
    if bytes.len() < 16 {
        return false;
    }
    // ISO BMFF brand check (ftyp + heic/heix/hevc/heif/mif1 brands).
    if &bytes[4..8] != b"ftyp" {
        return false;
    }
    let brand = &bytes[8..12];
    matches!(brand, b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1")
}

fn looks_like_exr(bytes: &[u8]) -> bool {
    // OpenEXR magic number: 76 2F 31 01
    bytes.len() >= 4 && bytes[0..4] == [0x76, 0x2F, 0x31, 0x01]
}

fn looks_like_video(bytes: &[u8]) -> bool {
    infer::get(bytes)
        .map(|k| k.mime_type().to_ascii_lowercase().starts_with("video/"))
        .unwrap_or(false)
}

fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[0..8] == [137, 80, 78, 71, 13, 10, 26, 10]
}

fn looks_like_tiff_container(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && ((bytes[0] == b'I' && bytes[1] == b'I' && bytes[2] == 42 && bytes[3] == 0)
            || (bytes[0] == b'M' && bytes[1] == b'M' && bytes[2] == 0 && bytes[3] == 42))
}

fn libav_has_video_stream(bytes: &[u8]) -> bool {
    let Ok(mut tmp) = NamedTempFile::new() else {
        return false;
    };
    if tmp.write_all(bytes).is_err() {
        return false;
    }

    let Ok(input) = ffmpeg::format::input(&tmp.path()) else {
        return false;
    };

    input
        .streams()
        .any(|s| s.parameters().medium() == ffmpeg::media::Type::Video)
}

fn parse_heic_irot_quadrants(bytes: &[u8]) -> Option<u8> {
    // HEIF irot property box payload stores angle in the low 2 bits:
    // 0,1,2,3 => 0,90,180,270 degrees counter-clockwise.
    if bytes.len() < 9 {
        return None;
    }

    let mut pos = 0usize;
    while pos + 9 <= bytes.len() {
        let rel = bytes[pos..].windows(4).position(|w| w == b"irot")?;
        let i = pos + rel;
        if i >= 4 {
            let size = u32::from_be_bytes([bytes[i - 4], bytes[i - 3], bytes[i - 2], bytes[i - 1]]) as usize;
            if size >= 9 && i + 5 <= bytes.len() {
                return Some(bytes[i + 4] & 0b11);
            }
        }
        pos = i + 4;
    }

    None
}

fn apply_heic_rotation(img: DynamicImage, irot_quadrants_ccw: u8) -> DynamicImage {
    match irot_quadrants_ccw & 0b11 {
        0 => img,
        // irot is CCW; DynamicImage rotation helpers are clockwise.
        1 => img.rotate270(),
        2 => img.rotate180(),
        3 => img.rotate90(),
        _ => img,
    }
}

struct DecodedImage {
    image: DynamicImage,
    source_width: u32,
    source_height: u32,
}

fn decode_with_libav(
    bytes: &[u8],
    prefer_smallest_stream: bool,
    decode_target: Option<(u32, u32)>,
) -> Result<DecodedImage, String> {
    ffmpeg::init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    let mut tmp = NamedTempFile::new().map_err(|e| format!("tmp file create failed: {e}"))?;
    tmp.write_all(bytes)
        .map_err(|e| format!("tmp file write failed: {e}"))?;

    let mut input = ffmpeg::format::input(&tmp.path())
        .map_err(|e| format!("ffmpeg open input failed: {e}"))?;

    let stream_index = select_video_stream_index(&input, prefer_smallest_stream)?;
    let (context_decoder, stream_time_base) = {
        let video_stream = input
            .stream(stream_index)
            .ok_or_else(|| "selected video stream not found".to_string())?;
        let time_base = video_stream.time_base();
        let context_decoder = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())
            .map_err(|e| format!("ffmpeg decoder context failed: {e}"))?;
        (context_decoder, time_base)
    };
    let mut decoder = context_decoder
        .decoder()
        .video()
        .map_err(|e| format!("ffmpeg video decoder failed: {e}"))?;

    let source_is_linear = matches!(
        decoder.color_transfer_characteristic(),
        ffmpeg::util::color::TransferCharacteristic::Linear
    ) || looks_like_exr(bytes);

    let duration_us = input.duration();
    let seek_seconds = if duration_us > 60_000_000 {
        10
    } else if duration_us > 5_000_000 {
        1
    } else {
        0
    };
    let enable_thumbnail_scan = looks_like_video(bytes) || duration_us > 2_000_000;
    let frame_budget: usize = if enable_thumbnail_scan { 20 } else { 1 };

    if seek_seconds > 0 {
        let _ = fast_seek_to_stream_keyframe(
            &mut input,
            stream_index,
            stream_time_base,
            seek_seconds as f64,
        );
        decoder.flush();
    }

    let (out_w, out_h) = if let Some((target_w, target_h)) = decode_target {
        scaled_cover_overscan_dimensions(decoder.width(), decoder.height(), target_w, target_h)
    } else {
        (decoder.width(), decoder.height())
    };

    let mut scaler = ffmpeg::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::RGB24,
        out_w,
        out_h,
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
    )
    .map_err(|e| format!("ffmpeg scaler create failed: {e}"))?;

    let mut sampled_frames = 0usize;
    let mut best: Option<(f32, DynamicImage)> = None;

    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }

        decoder
            .send_packet(&packet)
            .map_err(|e| format!("ffmpeg send_packet failed: {e}"))?;

        if let Some(mut img) = try_receive_decoded_rgb(&mut decoder, &mut scaler)? {
            if source_is_linear {
                apply_linear_to_srgb_in_place(&mut img);
            }

            sampled_frames += 1;
            if frame_budget == 1 {
                return Ok(DecodedImage {
                    image: img,
                    source_width: decoder.width(),
                    source_height: decoder.height(),
                });
            }

            let score = thumbnail_frame_score(&img);
            match &best {
                None => best = Some((score, img)),
                Some((best_score, _)) if score > *best_score => best = Some((score, img)),
                _ => {}
            }

            if sampled_frames >= frame_budget {
                let (_, image) = best.ok_or_else(|| "ffmpeg did not decode an image frame".to_string())?;
                return Ok(DecodedImage {
                    image,
                    source_width: decoder.width(),
                    source_height: decoder.height(),
                });
            }
        }
    }

    decoder
        .send_eof()
        .map_err(|e| format!("ffmpeg send_eof failed: {e}"))?;
    if let Some(mut img) = try_receive_decoded_rgb(&mut decoder, &mut scaler)? {
        if source_is_linear {
            apply_linear_to_srgb_in_place(&mut img);
        }

        if frame_budget == 1 {
            return Ok(DecodedImage {
                image: img,
                source_width: decoder.width(),
                source_height: decoder.height(),
            });
        }

        let score = thumbnail_frame_score(&img);
        match &best {
            None => best = Some((score, img)),
            Some((best_score, _)) if score > *best_score => best = Some((score, img)),
            _ => {}
        }
    }

    if let Some((_, image)) = best {
        return Ok(DecodedImage {
            image,
            source_width: decoder.width(),
            source_height: decoder.height(),
        });
    }

    Err("ffmpeg did not decode an image frame".to_string())
}

fn scaled_cover_overscan_dimensions(src_w: u32, src_h: u32, target_w: u32, target_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || target_w == 0 || target_h == 0 {
        return (src_w.max(1), src_h.max(1));
    }

    let overscale_w = ((target_w as f32) * 1.10).ceil() as u32;
    let overscale_h = ((target_h as f32) * 1.10).ceil() as u32;

    let scale_w = overscale_w as f32 / src_w as f32;
    let scale_h = overscale_h as f32 / src_h as f32;
    let scale = scale_w.max(scale_h);

    // Avoid enlarging small sources during decode; post-process can decide
    // whether and how to upscale for output.
    if scale >= 1.0 {
        return (src_w.max(1), src_h.max(1));
    }

    let new_w = ((src_w as f32) * scale).ceil() as u32;
    let new_h = ((src_h as f32) * scale).ceil() as u32;
    (new_w.max(1), new_h.max(1))
}

fn select_video_stream_index(
    input: &ffmpeg::format::context::Input,
    prefer_smallest_stream: bool,
) -> Result<usize, String> {
    if !prefer_smallest_stream {
        return input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .map(|s| s.index())
            .ok_or_else(|| "no video/image stream found".to_string());
    }

    let mut best: Option<(usize, u64)> = None;
    for stream in input.streams() {
        if stream.parameters().medium() != ffmpeg::media::Type::Video {
            continue;
        }

        let dims = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .ok()
            .and_then(|ctx| ctx.decoder().video().ok())
            .map(|dec| (dec.width(), dec.height()));

        let area = match dims {
            Some((w, h)) if w > 0 && h > 0 => w as u64 * h as u64,
            _ => u64::MAX,
        };

        match best {
            None => best = Some((stream.index(), area)),
            Some((_, cur_area)) if area < cur_area => best = Some((stream.index(), area)),
            _ => {}
        }
    }

    if let Some((idx, _)) = best {
        return Ok(idx);
    }

    input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .map(|s| s.index())
        .ok_or_else(|| "no video/image stream found".to_string())
}

fn try_receive_decoded_rgb(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut ffmpeg::software::scaling::Context,
) -> Result<Option<DynamicImage>, String> {
    let mut frame = ffmpeg::util::frame::Video::empty();
    match decoder.receive_frame(&mut frame) {
        Ok(()) => {
            let mut rgb = ffmpeg::util::frame::Video::empty();
            scaler
                .run(&frame, &mut rgb)
                .map_err(|e| format!("ffmpeg scale failed: {e}"))?;

            let w = rgb.width();
            let h = rgb.height();
            let stride = rgb.stride(0);
            let row_len = (w as usize) * 3;
            let src = rgb.data(0);
            let mut packed = vec![0u8; row_len * h as usize];

            for y in 0..h as usize {
                let src_off = y * stride;
                let dst_off = y * row_len;
                packed[dst_off..dst_off + row_len]
                    .copy_from_slice(&src[src_off..src_off + row_len]);
            }

            let img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(w, h, packed)
                .ok_or_else(|| "ffmpeg RGB frame packing failed".to_string())?;
            Ok(Some(DynamicImage::ImageRgb8(img)))
        }
        Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::util::error::EAGAIN => Ok(None),
        Err(ffmpeg::Error::Eof) => Ok(None),
        Err(e) => Err(format!("ffmpeg receive_frame failed: {e}")),
    }
}

fn apply_linear_to_srgb_in_place(img: &mut DynamicImage) {
    let mut rgb = img.to_rgb8();
    for p in rgb.pixels_mut() {
        p[0] = linear_u8_to_srgb_u8(p[0]);
        p[1] = linear_u8_to_srgb_u8(p[1]);
        p[2] = linear_u8_to_srgb_u8(p[2]);
    }
    *img = DynamicImage::ImageRgb8(rgb);
}

fn linear_u8_to_srgb_u8(v: u8) -> u8 {
    let x = v as f32 / 255.0;
    let y = if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    (y * 255.0).round().clamp(0.0, 255.0) as u8
}

fn thumbnail_frame_score(img: &DynamicImage) -> f32 {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w < 3 || h < 3 {
        return 0.0;
    }

    let stride = 4usize;
    let mut sum = 0.0f32;
    let mut count = 0usize;
    for y in (1..(h - 1) as usize).step_by(stride) {
        for x in (1..(w - 1) as usize).step_by(stride) {
            let p = rgb.get_pixel(x as u32, y as u32);
            let l = 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;

            let px = rgb.get_pixel((x + 1) as u32, y as u32);
            let py = rgb.get_pixel(x as u32, (y + 1) as u32);
            let lx = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
            let ly = 0.2126 * py[0] as f32 + 0.7152 * py[1] as f32 + 0.0722 * py[2] as f32;

            // Favor frames with moderate brightness and some edge detail.
            let edge = (lx - l).abs() + (ly - l).abs();
            let midtone = 1.0 - ((l / 255.0) - 0.5).abs() * 2.0;
            sum += edge * (0.5 + 0.5 * midtone.clamp(0.0, 1.0));
            count += 1;
        }
    }

    if count == 0 {
        0.0
    } else {
        sum / count as f32
    }
}

fn fast_seek_to_stream_keyframe(
    input: &mut ffmpeg::format::context::Input,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    seconds: f64,
) -> Result<(), String> {
    let tb_num = time_base.numerator() as f64;
    let tb_den = time_base.denominator() as f64;
    if tb_num <= 0.0 || tb_den <= 0.0 {
        return Err("invalid stream time base".to_string());
    }

    let ts = (seconds * (tb_den / tb_num)).round() as i64;
    let ret = unsafe {
        ffmpeg::ffi::av_seek_frame(
            input.as_mut_ptr(),
            stream_index as i32,
            ts,
            ffmpeg::ffi::AVSEEK_FLAG_BACKWARD,
        )
    };

    if ret < 0 {
        return Err(format!("ffmpeg keyframe seek failed: {}", ffmpeg::Error::from(ret)));
    }

    Ok(())
}
