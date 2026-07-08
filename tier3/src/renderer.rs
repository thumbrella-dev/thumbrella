//! Tier 3 in-process renderer — extends tier 2 with pluggable backends.
//!
//! # Architecture
//!
//! `Tier3Renderer` delegates all standard format rendering (Image, Video,
//! Vector) to [`tier2::Tier2Renderer`].  This ensures tier 3 always matches
//! tier 2 behaviour exactly — tier 2 continues to evolve independently and
//! tier 3 inherits every improvement automatically.
//!
//! Tier 3 adds its own backends for formats tier 2 does not handle:
//!
//! | Kind | Tier 2 | Tier 3 |
//! |------|--------|--------|
//! | Image | libav, image crate, raw preview, jxl, resvg | (same as tier 2) |
//! | Video | libav | (same as tier 2) |
//! | Vector | resvg | (same as tier 2) + optional inkscape subprocess |
//! | Document | (none) | dlopen: pdfium, subprocess: libreoffice |
//! | Geometry | (none) | subprocess: f3d / usdrecord |
//!
//! # Dispatch order
//!
//! For Document and Geometry, tier 3 tries backends in order:
//! 1. Shared-library backends (dlopen) — fastest, in-process
//! 2. Subprocess backends (CLI tools) — heavier, sandboxed
//!
//! For Image, Video, and Vector, tier 3 delegates directly to tier 2.
//!
//! # Subprocess rendering
//!
//! When a subprocess backend is used, the source media is staged to a
//! [`ScratchArena`] as a temp file, the
//! CLI tool is invoked with that path, and the output image is read back
//! from the arena.  All temp files are cleaned up when the arena is dropped.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tier1::InProcessRenderer;
use tier1::RenderCook;

//  Embedded Python scripts 

/// Embedded `usd_extract.py` — extracts triangulated mesh from USD/USDZ → OBJ.
const USD_EXTRACT_PY: &str = include_str!("usd_extract.py");

/// Embedded `sanitize_glb.py` — strips images/textures/materials from GLB.
const SANITIZE_GLB_PY: &str = include_str!("sanitize_glb.py");
use tier1::media::FileKind;
use tier1::renderer::{RenderOutput, apply_render_output};
use tier2::Tier2Renderer;

use crate::scratch::ScratchArena;

/// Emit a debug message only when raw logs are enabled (TBR_LOG=full).
macro_rules! tbr_debug {
    ($($arg:tt)*) => {
        if tier1::ux::show_raw_logs() {
            eprintln!($($arg)*);
        }
    };
}

// ============================================================================
// Tier3Renderer
// ============================================================================

/// Tier 3 renderer — extends tier 2 with pluggable document/geometry backends.
///
/// All standard format rendering (image, video, vector) is delegated to the
/// inner [`Tier2Renderer`].  Tier-3-specific backends are registered based on
/// the environment probe results from [`crate::env_check`].
pub struct Tier3Renderer {
    /// Tier 2 renderer for standard formats.
    tier2: Tier2Renderer,
}

impl Tier3Renderer {
    /// Create a new tier 3 renderer.
    pub fn new() -> Self {
        Self {
            tier2: Tier2Renderer::new(),
        }
    }

    /// Create a shared (Arc-wrapped) tier 3 renderer.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl Default for Tier3Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcessRenderer for Tier3Renderer {
    fn render<'a>(&'a self, cook: &'a mut dyn RenderCook) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let kind = cook.media_kind();
            let ext = cook.media_extension().unwrap_or("?").to_string();
            let cl = cook.content_length();
            tbr_debug!("[tier3] render: kind={kind:?}  ext={ext}  content_length={cl:?}");

            // On a handoff, skip tier2 entirely — the lower tier already
            // tried and determined it needs tier3.  Go straight to
            // tier3-specific backends (geometry, document, arithmetic JPEGs).
            let is_handoff = cook.is_handoff();

            match kind {
                Some(FileKind::Document) => render_document_tier3(cook, &ext).await,
                Some(FileKind::Geometry) => render_geometry_tier3(cook, &ext).await,
                Some(FileKind::Image) if matches!(ext.as_str(), "jpeg" | "jpg") => {
                    if render_image_tier3(cook, &ext).await {
                        true
                    } else if is_handoff {
                        false
                    } else {
                        self.tier2.render(cook).await
                    }
                }
                Some(FileKind::Image) => {
                    // Try tier2 first (libav is faster for common formats).
                    // If tier2 claims the format but fails to decode
                    // (has_render_image is false), fall back to the
                    // ffmpeg/oiio CLI path below.
                    let tier2_ok = !is_handoff && self.tier2.render(cook).await && cook.has_render_image();
                    if tier2_ok { true } else { render_image_ffmpeg_fallback(cook, &ext).await }
                }
                Some(FileKind::Video) => {
                    // Try tier2 first (libav is faster).  Fall back to
                    // ffmpeg CLI for formats libav can't handle.
                    if !is_handoff && self.tier2.render(cook).await {
                        true
                    } else {
                        render_video_ffmpeg_fallback(cook, &ext).await
                    }
                }
                _ if is_handoff => {
                    // Handoff for a format tier3 doesn't specifically handle.
                    false
                }
                _ => self.tier2.render(cook).await,
            }
        })
    }
}

// ============================================================================
// Tier-3-specific render dispatch
// ============================================================================

/// Render a JPEG image via tier-3-specific backends.
///
/// Arithmetic-coded JPEGs (SOF9) are not supported by libav's mjpeg
/// decoder.  Tier 3 uses ImageMagick which delegates to libjpeg-turbo
/// and handles all JPEG variants.
async fn render_image_tier3(cook: &mut dyn RenderCook, ext: &str) -> bool {
    if !is_arithmetic_jpeg_peek(cook) {
        return false;
    }

    let report = crate::env_check::cached_report();
    let has_magick = report
        .as_ref()
        .and_then(|r| r.backends.get("magick"))
        .map(|b| b.available)
        .unwrap_or(false);

    if !has_magick {
        return false;
    }

    let Some(mut reader) = cook.take_reader() else {
        return false;
    };
    let ext_owned = ext.to_string();

    let result = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        use std::io::Read;
        reader.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
        run_magick_image_decode(&buf, &ext_owned)
    })
    .await;

    match result {
        Ok(Ok(out)) => {
            apply_render_output(cook, out);
            true
        }
        Ok(Err(msg)) => {
            cook.fail_cook(&msg);
            true
        }
        Err(_) => {
            cook.fail_cook("magick panicked");
            true
        }
    }
}

/// Peek at already-cached bytes to detect arithmetic JPEG coding.
/// Does not consume the reader.
fn is_arithmetic_jpeg_peek(cook: &dyn RenderCook) -> bool {
    let Some(buf) = cook.peek_bytes(512) else {
        return false;
    };
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return false;
    }
    let mut i = 2;
    while i + 3 < buf.len() {
        if buf[i] == 0xFF {
            match buf[i + 1] {
                0x00 | 0xFF => {
                    i += 1;
                    continue;
                }
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

/// Run ImageMagick `convert` to decode an arithmetic JPEG to PNG, with
/// power-of-2 downscaling when the source is large.  Uses `identify` first
/// to get source dimensions and colour depth.
///
/// ImageMagick delegates to libjpeg-turbo which supports arithmetic coding.
fn run_magick_image_decode(bytes: &[u8], ext: &str) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(50 * 1024 * 1024).map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena
        .stage_bytes(bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    //  identify: get source dimensions
    let (src_w, src_h) = {
        let output = Command::new("gm")
            .arg("identify")
            .arg("-format")
            .arg("%w %h")
            .arg(&input_path)
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| format!("spawn gm identify: {e}"))?;

        if !output.status.success() {
            return Err(format!("gm identify exited with {}", output.status));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(format!("gm identify: unexpected output: {text}"));
        }
        let w: u32 = parts[0].parse().map_err(|_| format!("gm identify width: {text}"))?;
        let h: u32 = parts[1].parse().map_err(|_| format!("gm identify height: {text}"))?;
        (w, h)
    };

    //  compute power-of-2 downscale
    // Only scale down.  Keep at least 256 px on the short side for quality.
    // Scale factors are powers of 2 (2, 4, 8, …) for fast DCT-level resize
    // in the deliver step.
    let max_dim = src_w.max(src_h);
    let scale: u32 = if max_dim > 512 {
        let mut s = 1u32;
        while max_dim / (s * 2) >= 256 {
            s *= 2;
        }
        s
    } else {
        1
    };
    let resize_w = src_w / scale;
    let resize_h = src_h / scale;

    //  convert: decode + resize → PNG
    let output_path = arena.output_path("png");

    let mut cmd = Command::new("gm");
    cmd.arg("convert");
    let resize_arg = format!("{}x{}", resize_w, resize_h);
    let png_out = format!("PNG:{}", output_path.display());
    cmd.arg(&input_path)
        .arg("-resize")
        .arg(&resize_arg)
        .arg(&png_out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    // Do not apply sandbox wrappers here: xvfb-run + f3d is already running
    // entirely inside our container boundary and may fail under pre-exec
    // restrictions despite being otherwise valid.

    let status = cmd.status().map_err(|e| format!("spawn gm convert: {e}"))?;
    if !status.success() {
        return Err(format!("gm convert exited with {status}"));
    }

    let png_bytes = arena.read_output(&output_path).map_err(|e| format!("read output: {e}"))?;
    let img = image::load_from_memory(&png_bytes).map_err(|e| format!("decode PNG output: {e}"))?;

    Ok(RenderOutput {
        image: img,
        renderer: Some("magick".into()),
        codec: None,
        video_seek_secs: None,
        properties: Some(serde_json::json!({
            "width": src_w,
            "height": src_h,
        })),
    })
}

/// Check if an extension is best handled by oiiotool (skip tier2).
fn is_oiio_format(ext: &str) -> bool {
    matches!(
        ext,
        "exr"
            | "sxr"
            | "mxr"
            | "hdr"
            | "rgbe"
            | "dpx"
            | "cin"
            | "dds"
            | "fits"
            | "iff"
            | "pic"
            | "rla"
            | "zfile"
            | "sgi"
            | "rgb"
            | "rgba"
            | "bw"
            | "int"
            | "inta"
    )
}

/// Run ffmpeg CLI to decode an image or video frame to PNG, with properties
/// via ffprobe and power-of-2 downscaling.
///
/// For video: seeks to 1 second, extracts one frame.  Only the first 10 MiB
/// of the source are passed — ffmpeg can often extract frames from truncated
/// files.
fn run_ffmpeg_decode(bytes: &[u8], ext: &str, is_video: bool) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(100 * 1024 * 1024).map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena
        .stage_bytes(bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    //  ffprobe: get dimensions, colour depth, duration, audio channels 
    let (src_w, src_h, bits_per_pixel, duration_secs, channel_count) = {
        let output = Command::new("ffprobe")
            .arg("-v")
            .arg("quiet")
            .arg("-print_format")
            .arg("json")
            .arg("-show_streams")
            .arg("-show_format")
            .arg(&input_path)
            .output()
            .map_err(|e| format!("spawn ffprobe: {e}"))?;

        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|e| format!("ffprobe json: {e}"))?;
        let streams = json["streams"].as_array().ok_or_else(|| "ffprobe: no streams".to_string())?;

        //  video stream 
        let vs = streams.iter().find(|s| s["codec_type"] == "video");
        let (w, h, bpp) = if let Some(s) = vs {
            let w = s["width"].as_u64().unwrap_or(0) as u32;
            let h = s["height"].as_u64().unwrap_or(0) as u32;
            if w == 0 || h == 0 {
                return Err("ffprobe: zero dimensions".into());
            }
            let bpp = s["pix_fmt"].as_str().map(pix_fmt_bits_per_pixel).unwrap_or(0);
            (w, h, bpp)
        } else {
            return Err("ffprobe: no video stream".into());
        };

        //  audio streams
        let chan = streams
            .iter()
            .filter(|s| s["codec_type"] == "audio")
            .filter_map(|s| s["channels"].as_u64())
            .sum::<u64>() as u32;

        //  duration 
        let dur = json["format"]["duration"].as_str().and_then(|s| s.parse::<f64>().ok());

        (w, h, bpp, dur, chan)
    };

    //  power-of-2 downscale 
    let max_dim = src_w.max(src_h);
    let scale: u32 = if max_dim > 512 {
        let mut s = 1u32;
        while max_dim / (s * 2) >= 256 {
            s *= 2;
        }
        s
    } else {
        1
    };
    let resize_w = src_w / scale;
    let resize_h = src_h / scale;

    //  ffmpeg: decode + resize → PNG
    let output_path = arena.output_path("png");
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-v").arg("error");

    // Input seeking: -ss before -i seeks to nearest keyframe (fast).
    if is_video {
        cmd.arg("-ss").arg("1");
    }
    cmd.arg("-i").arg(&input_path);

    // Output filter: thumbnail (video) and/or scale (both).
    let needs_resize = resize_w != src_w || resize_h != src_h;
    if is_video {
        if needs_resize {
            cmd.arg("-vf").arg(format!("thumbnail=n=20,scale={resize_w}:{resize_h}"));
        } else {
            cmd.arg("-vf").arg("thumbnail=n=20");
        }
    } else if needs_resize {
        cmd.arg("-vf").arg(format!("scale={resize_w}:{resize_h}"));
    }

    cmd.arg("-frames:v")
        .arg("1")
        .arg(&output_path)
        .arg("-y")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    crate::sandbox::apply(&mut cmd, &crate::sandbox::default_strict());

    let output = cmd.output().map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("(no output)");
        return Err(format!("ffmpeg: {first_line}"));
    }

    let png_bytes = arena.read_output(&output_path).map_err(|e| format!("read output: {e}"))?;
    let mut img = image::load_from_memory(&png_bytes).map_err(|e| format!("decode PNG output: {e}"))?;

    // Scene-linear formats come through ffmpeg as linear 8-bit PNGs.
    // Apply a gamma-1.8 curve to approximate an sRGB display transform.
    // (1.8 is used instead of 2.2 because 8-bit truncation loses
    // highlight detail — a gentler curve compensates visually.)
    if is_linear_format(ext) {
        img = linear_to_srgb_fast(img);
    }

    Ok(RenderOutput {
        image: img,
        renderer: Some("ffmpeg_cli".into()),
        codec: None,
        video_seek_secs: if is_video { Some(1.0) } else { None },
        properties: Some(build_video_properties(
            src_w,
            src_h,
            bits_per_pixel,
            duration_secs,
            channel_count,
        )),
    })
}

/// Build a properties object for video/image content.  Fields are omitted
/// when the value is unknown (0 for integers, `None` for duration).
fn build_video_properties(
    w: u32,
    h: u32,
    bits_per_pixel: u32,
    duration_secs: Option<f64>,
    channel_count: u32,
) -> serde_json::Value {
    let mut props = serde_json::json!({
        "width": w,
        "height": h,
    });
    if bits_per_pixel > 0 {
        props["bpp"] = serde_json::json!(bits_per_pixel);
    }
    if let Some(d) = duration_secs {
        props["duration_seconds"] = serde_json::json!(d);
    }
    // channel_count is always written — 0 means "known to be silent".
    props["channel_count"] = serde_json::json!(channel_count);
    props
}

/// Approximate bits-per-pixel for a given ffmpeg `pix_fmt` string.
fn pix_fmt_bits_per_pixel(pix_fmt: &str) -> u32 {
    // Common 8-bit YUV subsampled formats.
    if pix_fmt.starts_with("yuv420")
        && !pix_fmt.contains("p10")
        && !pix_fmt.contains("p12")
        && !pix_fmt.contains("p16")
    {
        return 12;
    }
    if pix_fmt.starts_with("yuv422")
        && !pix_fmt.contains("p10")
        && !pix_fmt.contains("p12")
        && !pix_fmt.contains("p16")
    {
        return 16;
    }
    if pix_fmt.starts_with("yuv444")
        && !pix_fmt.contains("p10")
        && !pix_fmt.contains("p12")
        && !pix_fmt.contains("p16")
    {
        return 24;
    }
    // 10-bit YUV: multiply 8-bit value by 1.25.
    if pix_fmt.starts_with("yuv420p10") {
        return 15;
    }
    if pix_fmt.starts_with("yuv422p10") {
        return 20;
    }
    if pix_fmt.starts_with("yuv444p10") {
        return 30;
    }
    // 12-bit YUV.
    if pix_fmt.starts_with("yuv420p12") {
        return 18;
    }
    if pix_fmt.starts_with("yuv422p12") {
        return 24;
    }
    if pix_fmt.starts_with("yuv444p12") {
        return 36;
    }
    // RGB, BGR, GBR — 24 bits/pixel (8-bit) or more.
    if matches!(pix_fmt, "rgb24" | "bgr24" | "gbrp" | "gbrp9") {
        return 24;
    }
    if matches!(pix_fmt, "rgb48" | "bgr48") {
        return 48;
    }
    // Gray.
    if pix_fmt.starts_with("gray") {
        if pix_fmt.contains("10") {
            return 10;
        }
        if pix_fmt.contains("12") {
            return 12;
        }
        if pix_fmt.contains("16") {
            return 16;
        }
        return 8;
    }
    0
}

/// Tier-3 CLI fallback for image formats.  One tool per format — no
/// fallback chain within tier3.  Each format has a single best tool.
///
/// | Extension | Tool |
/// |-----------|------|
/// | exr, hdr, dpx, jp2, j2k | oiiotool |
/// | All others | ffmpeg CLI |
async fn render_image_ffmpeg_fallback(cook: &mut dyn RenderCook, ext: &str) -> bool {
    let report = crate::env_check::cached_report();

    // Pick the tool for this extension.
    let use_oiiotool = is_oiio_format(ext);
    let (tool_name, tool_available): (&str, bool) = if use_oiiotool {
        (
            "oiiotool",
            report
                .as_ref()
                .and_then(|r| r.backends.get("oiiotool"))
                .map(|b| b.available)
                .unwrap_or(false),
        )
    } else {
        (
            "ffmpeg_cli",
            report
                .as_ref()
                .and_then(|r| r.backends.get("ffmpeg_cli"))
                .map(|b| b.available)
                .unwrap_or(false),
        )
    };

    if !tool_available {
        return false;
    }

    let ext_owned = ext.to_string();

    // Two paths:
    // 1. Reader still available (tier2 didn't touch it) → read inside
    //    spawn_blocking because SyncHttpReader uses block_on.
    // 2. Reader consumed by tier2 → re-fetch bytes async, then spawn_blocking.
    if let Some(mut reader) = cook.take_reader() {
        let result = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            use std::io::Read;
            reader.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
            if use_oiiotool {
                run_oiiotool_decode(&buf, &ext_owned)
            } else {
                run_ffmpeg_decode(&buf, &ext_owned, false)
            }
        })
        .await;

        match result {
            Ok(Ok(out)) => {
                apply_render_output(cook, out);
                true
            }
            Ok(Err(msg)) => {
                cook.fail_cook(&format!("{tool_name}: {msg}"));
                true
            }
            Err(_) => {
                cook.fail_cook(&format!("{tool_name} panicked"));
                true
            }
        }
    } else {
        // Reader was consumed by a prior tier — re-fetch.
        let bytes = match tier2::renderer::fetch_url(cook.input_url()).await {
            Some(b) => b,
            None => {
                cook.fail_cook(&format!("{tool_name}: failed to re-fetch (reader consumed by prior tier)"));
                return true;
            }
        };
        let result = tokio::task::spawn_blocking(move || {
            if use_oiiotool {
                run_oiiotool_decode(&bytes, &ext_owned)
            } else {
                run_ffmpeg_decode(&bytes, &ext_owned, false)
            }
        })
        .await;

        match result {
            Ok(Ok(out)) => {
                apply_render_output(cook, out);
                true
            }
            Ok(Err(msg)) => {
                cook.fail_cook(&format!("{tool_name}: {msg}"));
                true
            }
            Err(_) => {
                cook.fail_cook(&format!("{tool_name} panicked"));
                true
            }
        }
    }
}

/// Tier-3 ffmpeg CLI fallback for video.  Reads only the first 10 MiB —
/// ffmpeg can often extract a keyframe from a truncated file.
async fn render_video_ffmpeg_fallback(cook: &mut dyn RenderCook, ext: &str) -> bool {
    let report = crate::env_check::cached_report();
    let has_ffmpeg = report
        .as_ref()
        .and_then(|r| r.backends.get("ffmpeg_cli"))
        .map(|b| b.available)
        .unwrap_or(false);
    if !has_ffmpeg {
        return false;
    }

    let Some(mut reader) = cook.take_reader() else {
        return false;
    };
    let ext_owned = ext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::with_capacity(10 * 1024 * 1024);
        use std::io::Read;
        // Read at most 10 MiB — ffmpeg works with truncated files.
        let mut chunk = [0u8; 8192];
        loop {
            let n = reader.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() >= 10 * 1024 * 1024 {
                break;
            }
        }
        run_ffmpeg_decode(&buf, &ext_owned, true)
    })
    .await;

    match result {
        Ok(Ok(out)) => {
            apply_render_output(cook, out);
            true
        }
        Ok(Err(msg)) => {
            cook.fail_cook(&msg);
            true
        }
        Err(_) => {
            cook.fail_cook("ffmpeg_cli panicked");
            true
        }
    }
}
/// Run OpenImageIO `oiiotool` to decode a studio-format image to PNG, with
/// properties via `--info` and power-of-2 downscaling.  Handles EXR, HDR,
/// DPX, and other formats that ffmpeg/magick may struggle with.
fn run_oiiotool_decode(bytes: &[u8], ext: &str) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(100 * 1024 * 1024).map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena
        .stage_bytes(bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    //  oiiotool --info: get dimensions 
    let (src_w, src_h) = {
        let output = Command::new("oiiotool")
            .arg("--info")
            .arg("-v")
            .arg(&input_path)
            .output()
            .map_err(|e| format!("spawn oiiotool: {e}"))?;

        let text = String::from_utf8_lossy(&output.stdout);
        // Parse "filename : 1262 x  860, ..." from the second line.
        let mut w = 0u32;
        let mut h = 0u32;
        for line in text.lines() {
            if let Some(idx) = line.find(" : ") {
                let rest = &line[idx + 3..]; // after " : "
                if let Some(comma) = rest.find(',') {
                    let dims = &rest[..comma]; // "1262 x  860"
                    let parts: Vec<&str> = dims.split('x').map(|s| s.trim()).collect();
                    if parts.len() >= 2 {
                        w = parts[0].parse().unwrap_or(0);
                        h = parts[1].parse().unwrap_or(0);
                    }
                }
                break;
            }
        }
        if w == 0 || h == 0 {
            return Err(format!("oiiotool: could not parse dimensions from: {text}"));
        }
        (w, h)
    };

    //  power-of-2 downscale 
    let mut resize_w = src_w;
    let mut resize_h = src_h;
    let max_dim = src_w.max(src_h);
    if max_dim > 512 {
        let mut s = 1u32;
        while max_dim / (s * 2) >= 256 {
            s *= 2;
        }
        resize_w = src_w / s;
        resize_h = src_h / s;
    }

    //  oiiotool: decode + colorspace + resize → PNG
    let output_path = arena.output_path("png");
    let mut cmd = Command::new("oiiotool");
    cmd.arg(&input_path).arg("--colorconvert").arg("linear").arg("sRGB");
    if resize_w != src_w || resize_h != src_h {
        cmd.arg("--resize").arg(format!("{resize_w}x{resize_h}"));
    }
    cmd.arg("-o")
        .arg(&output_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // No sandbox — oiiotool is a trusted system tool.

    let output = cmd.output().map_err(|e| format!("spawn oiiotool: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("(no output)");
        return Err(format!("oiiotool: {first_line}"));
    }

    let png_bytes = arena.read_output(&output_path).map_err(|e| format!("read output: {e}"))?;
    let img = image::load_from_memory(&png_bytes).map_err(|e| format!("decode PNG output: {e}"))?;

    Ok(RenderOutput {
        image: img,
        renderer: Some("oiiotool".into()),
        codec: None,
        video_seek_secs: None,
        properties: Some(serde_json::json!({
            "width": src_w,
            "height": src_h,
        })),
    })
}
/// Render a document via tier-3-specific backends.
///
/// Dispatch order:
/// 1. Shared-library backends (dlopen) — e.g. libpdfium.
/// 2. Subprocess backends — e.g. libreoffice headless.
async fn render_document_tier3(cook: &mut dyn RenderCook, ext: &str) -> bool {
    // Stub: document rendering will use dlopen (pdfium) or subprocess
    // (libreoffice --headless) to render the first page.
    let _ = (cook, ext);
    false
}

/// Render 3D geometry via tier-3-specific backends.
///
/// Dispatch is extension-based — each registered handler declares which
/// extensions it handles.  Unrecognised extensions fall through to a
/// placeholder.
///
/// Dispatch order:
/// 1. Walk registered handlers, find first matching extension, invoke it.
async fn render_geometry_tier3(cook: &mut dyn RenderCook, ext: &str) -> bool {
    // USDZ/USDC/USDA: extract mesh to OBJ via usd-core Python script,
    // then render the OBJ through the existing F3D handler (which provides
    // tone mapping, camera orbit, and delivery identical to STL/OBJ).
    if matches!(ext, "usdz" | "usdc" | "usda") {
        if let Some(mut reader) = cook.take_reader() {
            let result = tokio::task::spawn_blocking(move || run_usdz_via_obj_handler(&mut *reader)).await;

            match result {
                Ok(Ok(out)) => {
                    apply_render_output(cook, out);
                    return true;
                }
                Ok(Err(msg)) => {
                    tbr_debug!("[tier3] usdz: {msg}");
                    cook.fail_cook(&format!("usdz: {msg}"));
                    return true;
                }
                Err(_) => {
                    cook.fail_cook("usdz: panicked");
                    return true;
                }
            }
        }
        return false;
    }

    // Find the first registered handler that claims this extension.
    let handlers = crate::env_check::registered_handlers();
    let Some(handler) = handlers.iter().find(|h| h.extensions.contains(&ext)) else {
        return false;
    };

    // Check that the handler passed its availability probe.
    let report = crate::env_check::cached_report();
    let available = report
        .as_ref()
        .and_then(|r| r.backends.get(handler.name))
        .map(|b| b.available)
        .unwrap_or(false);

    if !available {
        return false;
    }

    // Use direct F3D invocation for the common geometry path.
    if handler.command == "f3d" {
        if let Some(mut reader) = cook.take_reader() {
            let ext_owned = ext.to_string();
            let result =
                tokio::task::spawn_blocking(move || run_f3d_geometry_handler(&mut *reader, &ext_owned)).await;

            match result {
                Ok(Ok(out)) => {
                    apply_render_output(cook, out);
                    return true;
                }
                Ok(Err(msg)) => {
                    tbr_debug!("[tier3] {}: {msg}", handler.name);
                    cook.fail_cook(&format!("{}: {msg}", handler.name));
                    return true;
                }
                Err(_) => {
                    cook.fail_cook(&format!("{}: panicked", handler.name));
                    return true;
                }
            }
        }

        return false;
    }

    // Try the subprocess backend.
    if let Some(mut reader) = cook.take_reader() {
        let cmd = handler.command.to_string();
        let name = handler.name.to_string();
        let ext_owned = ext.to_string();
        let result =
            tokio::task::spawn_blocking(move || run_subprocess_handler(&mut *reader, &ext_owned, &cmd)).await;

        match result {
            Ok(Ok(out)) => {
                apply_render_output(cook, out);
                return true;
            }
            Ok(Err(msg)) => {
                // Renderer ran but failed — log the message, fall through
                // to placeholder.
                tbr_debug!("[tier3] {name}: {msg}");
                cook.fail_cook(&format!("{name}: {msg}"));
                return true;
            }
            Err(_) => {
                cook.fail_cook(&format!("{name}: panicked"));
                return true;
            }
        }
    }

    false
}

/// Run a generic subprocess handler: stage input, invoke command, decode output.
///
/// This is synchronous and CPU-light (mostly process I/O).  Callers should
/// invoke it via `tokio::task::spawn_blocking`.
///
/// The handler command receives three arguments:
///   1. input path (staged source file)
///   2. output path (JPEG to write)
///   3. properties path (JSON to write)
fn run_subprocess_handler(
    reader: &mut dyn tier1::ReadSeek,
    ext: &str,
    command: &str,
) -> Result<RenderOutput, String> {
    use std::process::Command;

    // Ensure subprocess handlers receive the full source file.
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("rewind input: {e}"))?;

    // Create a scratch arena for this invocation.
    let arena = ScratchArena::new(100 * 1024 * 1024) // 100 MiB limit
        .map_err(|e| format!("scratch arena: {e}"))?;

    // Stage the source to a temp file.
    let input_path = arena
        .stage_reader(reader, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    // Allocate output paths.
    let output_path = arena.output_path("jpg");
    let props_path = arena.output_path("json");

    // Run the renderer in a sandboxed subprocess.
    let mut cmd = Command::new(command);
    cmd.arg(&input_path)
        .arg(&output_path)
        .arg(&props_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    crate::sandbox::apply(&mut cmd, &crate::sandbox::default_strict());

    let status = cmd.status().map_err(|e| format!("spawn {command}: {e}"))?;

    if !status.success() {
        return Err(format!("exited with {status}"));
    }

    // Read back the rendered image.
    let jpeg_bytes = arena.read_output(&output_path).map_err(|e| format!("read output: {e}"))?;

    // Decode the JPEG output.
    let img = image::load_from_memory(&jpeg_bytes).map_err(|e| format!("decode output JPEG: {e}"))?;

    // Read the renderer-produced properties.
    let props = std::fs::read_to_string(&props_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());

    // Arena is dropped here — temp files cleaned up.

    Ok(RenderOutput {
        image: img,
        renderer: Some(command.to_string()),
        codec: None,
        video_seek_secs: None,
        properties: props,
    })
}

/// Extract mesh from USDZ/USDC/USDA to OBJ, then render via the existing
/// F3D handler (which applies tone mapping, camera orbit, and delivery).
fn run_usdz_via_obj_handler(reader: &mut dyn tier1::ReadSeek) -> Result<RenderOutput, String> {
    use std::process::Command;

    // Read the full USDZ data.
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("rewind usdz: {e}"))?;
    let mut usdz_bytes = Vec::new();
    reader.read_to_end(&mut usdz_bytes).map_err(|e| format!("read usdz: {e}"))?;

    let arena = ScratchArena::new(100 * 1024 * 1024).map_err(|e| format!("scratch arena: {e}"))?;

    // Stage the USDZ to a temp file so the Python script can open it.
    let usdz_path = arena
        .stage_bytes(&usdz_bytes, "input.usdz")
        .map_err(|e| format!("stage usdz: {e}"))?;

    let obj_path = arena.output_path("obj");

    // Stage the embedded usd_extract.py script to a temp file.
    let script_path = arena
        .stage_bytes(USD_EXTRACT_PY.as_bytes(), "usd_extract.py")
        .map_err(|e| format!("stage usd_extract script: {e}"))?;

    // Run usd_extract.py (embedded) to convert USDZ → OBJ.
    let extract_status = Command::new("python3")
        .arg(&script_path)
        .arg(&usdz_path)
        .arg(&obj_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|e| format!("spawn usd_extract: {e}"))?;

    if !extract_status.success() {
        return Err(format!("usd_extract exited with {extract_status}"));
    }

    // Read the extracted OBJ.
    let obj_bytes = std::fs::read(&obj_path).map_err(|e| format!("read obj output: {e}"))?;

    if obj_bytes.is_empty() {
        return Err("usd_extract produced empty OBJ".into());
    }

    // Render the OBJ through the existing F3D handler.
    let mut cursor = std::io::Cursor::new(obj_bytes);
    run_f3d_geometry_handler(&mut cursor, "obj")
}

/// Run F3D directly to render STL/OBJ/glTF/GLB geometry to a PNG, then
/// load the resulting image into a RenderOutput.
fn run_f3d_geometry_handler(reader: &mut dyn tier1::ReadSeek, ext: &str) -> Result<RenderOutput, String> {
    use std::process::{Command, Stdio};

    // Read the full source so we can sanitise GLB files before staging.
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("rewind input: {e}"))?;

    let mut source_bytes = Vec::new();
    reader.read_to_end(&mut source_bytes).map_err(|e| format!("read input: {e}"))?;

    // STL/OBJ assets are commonly authored Z-up, while glTF assets are
    // typically Y-up. This is only a default hint; camera transforms still
    // apply after load.
    let up_axis = match ext {
        "stl" | "obj" => "+Z",
        _ => "+Y",
    };

    // Baseline framing and material styling for thumbnail appeal.
    const AZIMUTH: &str = "-30";
    const ELEVATION: &str = "20";
    const VIEW_ANGLE: &str = "45";
    const RESOLUTION: &str = "512,512";
    const BASE_COLOR: &str = "1,.9,.5";

    let arena = ScratchArena::new(100 * 1024 * 1024).map_err(|e| format!("scratch arena: {e}"))?;

    // Stage the source, then sanitise GLB files in-place (VTK 9.1 crashes
    // on embedded textures and KHR_texture_transform).
    let input_path = arena
        .stage_bytes(&source_bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    if ext == "glb" {
        // Stage the embedded sanitize_glb.py script to a temp file.
        let script_path = arena
            .stage_bytes(SANITIZE_GLB_PY.as_bytes(), "sanitize_glb.py")
            .map_err(|e| format!("stage sanitize script: {e}"))?;

        let output = std::process::Command::new("python3")
            .arg(&script_path)
            .arg(&input_path)
            .output()
            .map_err(|e| format!("sanitize_glb: {e}"))?;
        if !output.status.success() {
            return Err("sanitize_glb.py failed".into());
        }
        std::fs::write(&input_path, &output.stdout).map_err(|e| format!("write sanitized glb: {e}"))?;
    }

    let output_path = arena.output_path("png");

    // F3D 2.2.x (Debian) does not support newer flags like --no-config or
    // --rendering-backend. Use broadly-compatible options.
    let mut cmd = if std::env::var("DISPLAY").is_ok() {
        let mut c = Command::new("f3d");
        c.arg(&input_path)
            .arg(format!("--output={}", output_path.display()))
            .arg("--dry-run")
            .arg(format!("--resolution={}", RESOLUTION))
            .arg("--quiet")
            .arg("--no-background")
            .arg("--up")
            .arg(up_axis)
            .arg("--color")
            .arg(BASE_COLOR)
            .arg("--camera-azimuth-angle")
            .arg(AZIMUTH)
            .arg("--camera-elevation-angle")
            .arg(ELEVATION)
            .arg("--camera-view-angle")
            .arg(VIEW_ANGLE);
        c
    } else {
        let mut c = Command::new("xvfb-run");
        #[allow(clippy::suspicious_command_arg_space)]
        c.arg("-a")
            .arg("-s")
            .arg("-screen 0 1600x1200x24")
            .arg("f3d")
            .arg(&input_path)
            .arg(format!("--output={}", output_path.display()))
            .arg("--dry-run")
            .arg(format!("--resolution={}", RESOLUTION))
            .arg("--quiet")
            .arg("--no-background")
            .arg("--up")
            .arg(up_axis)
            .arg("--color")
            .arg(BASE_COLOR)
            .arg("--camera-azimuth-angle")
            .arg(AZIMUTH)
            .arg("--camera-elevation-angle")
            .arg(ELEVATION)
            .arg("--camera-view-angle")
            .arg(VIEW_ANGLE);
        c
    };

    cmd.stdout(Stdio::null()).stderr(Stdio::piped());

    // Do not apply the strict sandbox here: F3D rendering under Xvfb requires
    // GL/X11 process setup that may be terminated by the default bwrap profile.

    let output = cmd.output().map_err(|e| format!("spawn f3d: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first_err = stderr.lines().next().unwrap_or("");
        let first_out = stdout.lines().next().unwrap_or("");
        let detail = if !first_err.is_empty() {
            first_err
        } else if !first_out.is_empty() {
            first_out
        } else {
            "(no stderr/stdout)"
        };
        return Err(format!("f3d exited {:?}: {detail}", output.status.code()));
    }

    let png_bytes = arena.read_output(&output_path).map_err(|e| format!("read output: {e}"))?;

    let img = image::load_from_memory(&png_bytes).map_err(|e| format!("decode output PNG: {e}"))?;

    // Only apply the project tone map to formats that are typically unmaterialed.
    // For textured/material-rich formats (for example FBX/glTF/USD), preserve
    // source colors by skipping this remap.
    let img = if should_apply_geometry_tonemap(ext) {
        stylize_f3d_image(img).adjust_contrast(14.0)
    } else {
        img
    };

    // Autocrop transparent borders so the deliver step gets a tight
    // bounding box before resizing to the canonical thumbnail size.
    let img = autocrop_transparent(img);

    let props = serde_json::json!({
        "width": img.width(),
        "height": img.height(),
        "up_axis": up_axis,
        "camera_azimuth_deg": -30,
        "camera_elevation_deg": 20,
        "base_color": BASE_COLOR,
    });

    Ok(RenderOutput {
        image: img,
        renderer: Some("f3d".into()),
        codec: None,
        video_seek_secs: None,
        properties: Some(props),
    })
}

/// Apply a purple→gold gradient map to F3D output while preserving alpha.
///
/// This gives low-contrast geometry renders a stronger visual identity before
/// tier1 composites them onto the canonical background.
fn stylize_f3d_image(img: image::DynamicImage) -> image::DynamicImage {
    let mut rgba = img.to_rgba8();

    // Purple is restricted to the deepest tones; mids/highs stay warm gold.
    let shadow_purple = [84.0f32, 54.0, 146.0];
    let warm_mid = [184.0f32, 150.0, 100.0];
    // Pulled back from [242,208,150] to prevent highlight blowout
    // on light-coloured models (e.g. pale vintage cars).
    let warm_high = [210.0f32, 185.0, 140.0];

    for px in rgba.pixels_mut() {
        let a = px[3];
        if a == 0 {
            continue;
        }

        let r = px[0] as f32 / 255.0;
        let g = px[1] as f32 / 255.0;
        let b = px[2] as f32 / 255.0;

        // Start from scene luminance and apply a strong contrast curve.
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let t = ((luma - 0.5) * 1.28 + 0.5).clamp(0.0, 1.0);

        // Warm body tones for most of the range.
        let warm_t = ((t - 0.14) / 0.86).clamp(0.0, 1.0);
        let wr = warm_mid[0] + (warm_high[0] - warm_mid[0]) * warm_t;
        let wg = warm_mid[1] + (warm_high[1] - warm_mid[1]) * warm_t;
        let wb = warm_mid[2] + (warm_high[2] - warm_mid[2]) * warm_t;

        // Keep purple only in deep shadows.
        let shadow_t = ((0.58 - t) / 0.58).clamp(0.0, 1.0);
        let shadow_shape = shadow_t * shadow_t * (3.0 - 2.0 * shadow_t);
        let shadow_mix = 1.0 * shadow_shape;

        // Add an extra cool push in the deepest shadows so low-end tones
        // cannot collapse to brown/amber only.
        let deep_t = ((0.30 - t) / 0.30).clamp(0.0, 1.0);
        let deep_boost = deep_t * deep_t;

        // Global warmth lift across the model, with restrained highlight gain.
        let lift_t = ((t - 0.10) / 0.90).clamp(0.0, 1.0);
        // Soft-ceiling the lift so highlights don't blow past 255.
        let lift = 1.01 + 0.20 * (lift_t * lift_t * (3.0 - 2.0 * lift_t));

        // Preserve form: keep dark regions denser and prevent flat washout.
        let shade = 0.80 + 0.30 * t;

        // Push shadows cooler: reduce red and raise blue as luminance drops.
        let cool_red = 16.0 * shadow_mix + 18.0 * deep_boost;
        let cool_blue = 26.0 * shadow_mix + 28.0 * deep_boost;

        let nr = ((wr * (1.0 - shadow_mix) + shadow_purple[0] * shadow_mix - cool_red) * lift * shade)
            .clamp(0.0, 255.0);
        let ng = ((wg * (1.0 - shadow_mix) + shadow_purple[1] * shadow_mix) * lift * shade).clamp(0.0, 255.0);
        let nb = ((wb * (1.0 - shadow_mix) + shadow_purple[2] * shadow_mix + cool_blue) * lift * shade)
            .clamp(0.0, 255.0);

        px[0] = nr as u8;
        px[1] = ng as u8;
        px[2] = nb as u8;
    }

    image::DynamicImage::ImageRgba8(rgba)
}

/// Heuristic gate for aggressive color remapping.
///
/// STL/OBJ frequently arrive without authored materials, so the custom
/// purple→gold grade improves readability.  Most other geometry formats often
/// carry materials/textures that should be preserved.
fn should_apply_geometry_tonemap(ext: &str) -> bool {
    matches!(ext, "stl" | "obj")
}


//  Linear → sRGB helpers (shared by ffmpeg_cli and oiiotool paths) 

/// Returns `true` for extensions that are scene-linear (needs gamma correction).
fn is_linear_format(ext: &str) -> bool {
    matches!(ext, "exr" | "hdr" | "rgbe" | "sxr" | "mxr")
}

/// Fast gamma-2.2 correction for scene-linear pixel data.
///
/// Scene-linear formats store light proportionally.  Without tone-mapping
/// they render dark and crushed on an sRGB display.  This applies a simple
/// power-law curve so the deliver step receives perceptually-correct pixels.
fn linear_to_srgb_fast(img: image::DynamicImage) -> image::DynamicImage {
    // Apply gamma 2.2 to bring linear-light pixel data into approximate
    // sRGB perceptual space.  The ffmpeg CLI path converts HDR to 8-bit
    // PNG first, so highlight detail is already clipped — this is a
    // best-effort correction, not a full colour pipeline.
    const GAMMA: f64 = 1.0 / 2.2;

    fn gamma_pixel_rgb(p: &mut image::Rgb<u8>) {
        for c in &mut p.0 {
            let linear = *c as f64 / 255.0;
            *c = ((linear.powf(GAMMA)) * 255.0).round() as u8;
        }
    }

    fn gamma_pixel_rgba(p: &mut image::Rgba<u8>) {
        for c in 0..3 {
            let linear = p.0[c] as f64 / 255.0;
            p.0[c] = ((linear.powf(GAMMA)) * 255.0).round() as u8;
        }
    }

    match img {
        image::DynamicImage::ImageRgb8(mut buf) => {
            buf.pixels_mut().for_each(gamma_pixel_rgb);
            image::DynamicImage::ImageRgb8(buf)
        }
        image::DynamicImage::ImageRgba8(mut buf) => {
            buf.pixels_mut().for_each(gamma_pixel_rgba);
            image::DynamicImage::ImageRgba8(buf)
        }
        other => other,
    }
}

fn autocrop_transparent(img: image::DynamicImage) -> image::DynamicImage {
    const BORDER: u32 = 8;

    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());

    // Find the first non-transparent row from the top.
    let top = (0..h)
        .find(|&y| rgba.rows().nth(y as usize).is_some_and(|mut row| row.any(|p| p[3] != 0)))
        .unwrap_or(0);

    // First non-transparent row from the bottom.
    let bottom = (0..h)
        .rev()
        .find(|&y| rgba.rows().nth(y as usize).is_some_and(|mut row| row.any(|p| p[3] != 0)))
        .unwrap_or(h - 1);

    // First non-transparent column from the left.
    let left = (0..w).find(|&x| (0..h).any(|y| rgba.get_pixel(x, y)[3] != 0)).unwrap_or(0);

    // First non-transparent column from the right.
    let right = (0..w)
        .rev()
        .find(|&x| (0..h).any(|y| rgba.get_pixel(x, y)[3] != 0))
        .unwrap_or(w - 1);

    if top >= bottom || left >= right {
        return img; // fully transparent — passthrough
    }

    // Back off by BORDER pixels, clamped to image bounds.
    let top = top.saturating_sub(BORDER);
    let bottom = (bottom + BORDER).min(h - 1);
    let left = left.saturating_sub(BORDER);
    let right = (right + BORDER).min(w - 1);

    let cropped = image::imageops::crop_imm(&rgba, left, top, right - left + 1, bottom - top + 1);
    image::DynamicImage::ImageRgba8(cropped.to_image())
}
