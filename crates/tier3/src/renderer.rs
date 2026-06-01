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
//! | Geometry | (none) | subprocess: blender |
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
//! [`ScratchArena`](crate::scratch::ScratchArena) as a temp file, the
//! CLI tool is invoked with that path, and the output image is read back
//! from the arena.  All temp files are cleaned up when the arena is dropped.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tier1::InProcessRenderer;
use tier1::RenderCook;
use tier1::media::FileKind;
use tier1::renderer::{RenderOutput, apply_render_output};
use tier2::Tier2Renderer;

use crate::scratch::ScratchArena;

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
    /// Environment capability report from startup probe.
    #[allow(dead_code)]
    env: crate::env_check::EnvReport,
}

impl Tier3Renderer {
    /// Create a new tier 3 renderer, probing the environment for available
    /// backends.
    pub fn new() -> Self {
        Self {
            tier2: Tier2Renderer::new(),
            env: crate::env_check::probe_environment(),
        }
    }

    /// Create a shared (Arc-wrapped) tier 3 renderer.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl Default for Tier3Renderer {
    fn default() -> Self { Self::new() }
}

impl InProcessRenderer for Tier3Renderer {
    fn render<'a>(
        &'a self,
        cook: &'a mut dyn RenderCook,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let kind = cook.media_kind();
            let ext  = cook.media_extension().unwrap_or("?").to_string();
            let cl   = cook.content_length();
            eprintln!("[tier3] render: kind={kind:?}  ext={ext}  content_length={cl:?}");

            // On a handoff, skip tier2 entirely — the lower tier already
            // tried and determined it needs tier3.  Go straight to
            // tier3-specific backends (geometry, document, arithmetic JPEGs).
            let is_handoff = cook.is_handoff();

            match kind {
                Some(FileKind::Document) => {
                    render_document_tier3(cook, &ext).await
                }
                Some(FileKind::Geometry) => {
                    render_geometry_tier3(cook, &ext).await
                }
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
                    // Fall back to ffmpeg CLI for formats libav can't handle.
                    if !is_handoff && self.tier2.render(cook).await {
                        true
                    } else {
                        render_image_ffmpeg_fallback(cook, &ext).await
                    }
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
                _ => {
                    self.tier2.render(cook).await
                }
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
async fn render_image_tier3(
    cook: &mut dyn RenderCook,
    ext: &str,
) -> bool {
    if !is_arithmetic_jpeg_peek(cook) {
        return false;
    }

    let report = crate::env_check::cached_report();
    let has_magick = report.as_ref()
        .and_then(|r| r.backends.get("magick"))
        .map(|b| b.available).unwrap_or(false);

    if !has_magick {
        return false;
    }

    let Some(mut reader) = cook.take_reader() else { return false; };
    let ext_owned = ext.to_string();

    let result = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        use std::io::Read;
        reader.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
        run_magick_image_decode(&buf, &ext_owned)
    }).await;

    match result {
        Ok(Ok(out)) => { apply_render_output(cook, out); true }
        Ok(Err(msg)) => { cook.fail_cook(&msg); true }
        Err(_) => { cook.fail_cook("magick panicked"); true }
    }
}

/// Peek at already-cached bytes to detect arithmetic JPEG coding.
/// Does not consume the reader.
fn is_arithmetic_jpeg_peek(cook: &dyn RenderCook) -> bool {
    let Some(buf) = cook.peek_bytes(512) else { return false; };
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return false;
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

/// Run ImageMagick `convert` to decode an arithmetic JPEG to PNG, with
/// power-of-2 downscaling when the source is large.  Uses `identify` first
/// to get source dimensions and colour depth.
///
/// ImageMagick delegates to libjpeg-turbo which supports arithmetic coding.
fn run_magick_image_decode(
    bytes: &[u8],
    ext: &str,
) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(50 * 1024 * 1024)
        .map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena.stage_bytes(bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    // ── identify: get source dimensions ───────────────────────────────────
    let (src_w, src_h) = {
        let output = Command::new("gm")
            .arg("identify")
            .arg("-format").arg("%w %h")
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

    // ── compute power-of-2 downscale ───────────────────────────────────────
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

    // ── convert: decode + resize → PNG ─────────────────────────────────────
    let output_path = arena.output_path("png");

    let mut cmd = Command::new("gm");
    cmd.arg("convert");
    let resize_arg = format!("{}x{}", resize_w, resize_h);
    let png_out = format!("PNG:{}", output_path.display());
    cmd.arg(&input_path)
       .arg("-resize").arg(&resize_arg)
       .arg(&png_out)
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::piped());

    crate::sandbox::apply(&mut cmd, &crate::sandbox::default_strict());

    let status = cmd.status().map_err(|e| format!("spawn gm convert: {e}"))?;
    if !status.success() {
        return Err(format!("gm convert exited with {status}"));
    }

    let png_bytes = arena.read_output(&output_path)
        .map_err(|e| format!("read output: {e}"))?;
    let img = image::load_from_memory(&png_bytes)
        .map_err(|e| format!("decode PNG output: {e}"))?;

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
    matches!(ext,
        "exr" | "sxr" | "mxr" | "hdr" | "rgbe" | "dpx" | "cin"
        | "dds" | "fits" | "iff" | "pic" | "rla" | "zfile"
        | "sgi" | "rgb" | "rgba" | "bw" | "int" | "inta"
    )
}

/// Run ffmpeg CLI to decode an image or video frame to PNG, with properties
/// via ffprobe and power-of-2 downscaling.
///
/// For video: seeks to 1 second, extracts one frame.  Only the first 10 MiB
/// of the source are passed — ffmpeg can often extract frames from truncated
/// files.
fn run_ffmpeg_decode(
    bytes: &[u8],
    ext: &str,
    is_video: bool,
) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(100 * 1024 * 1024)
        .map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena.stage_bytes(bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    // ── ffprobe: get dimensions ────────────────────────────────────────────
    let (src_w, src_h) = {
        let output = Command::new("ffprobe")
            .arg("-v").arg("quiet")
            .arg("-print_format").arg("json")
            .arg("-show_streams")
            .arg("-select_streams").arg("v:0")
            .arg(&input_path)
            .output()
            .map_err(|e| format!("spawn ffprobe: {e}"))?;

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("ffprobe json: {e}"))?;
        let streams = json["streams"].as_array()
            .ok_or_else(|| "ffprobe: no streams".to_string())?;
        if streams.is_empty() {
            return Err("ffprobe: no video stream".into());
        }
        let s = &streams[0];
        let w = s["width"].as_u64().unwrap_or(0) as u32;
        let h = s["height"].as_u64().unwrap_or(0) as u32;
        if w == 0 || h == 0 {
            return Err("ffprobe: zero dimensions".into());
        }
        (w, h)
    };

    // ── power-of-2 downscale ──────────────────────────────────────────────
    let max_dim = src_w.max(src_h);
    let scale: u32 = if max_dim > 512 {
        let mut s = 1u32;
        while max_dim / (s * 2) >= 256 { s *= 2; }
        s
    } else { 1 };
    let resize_w = src_w / scale;
    let resize_h = src_h / scale;

    // ── ffmpeg: decode + resize → PNG ─────────────────────────────────────
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

    cmd.arg("-frames:v").arg("1")
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

    let png_bytes = arena.read_output(&output_path)
        .map_err(|e| format!("read output: {e}"))?;
    let img = image::load_from_memory(&png_bytes)
        .map_err(|e| format!("decode PNG output: {e}"))?;

    Ok(RenderOutput {
        image: img,
        renderer: Some("ffmpeg_cli".into()),
        codec: None,
        video_seek_secs: if is_video { Some(1.0) } else { None },
        properties: Some(serde_json::json!({
            "width": src_w,
            "height": src_h,
        })),
    })
}

/// Tier-3 CLI fallback for image formats.  One tool per format — no
/// fallback chain within tier3.  Each format has a single best tool.
///
/// | Extension | Tool |
/// |-----------|------|
/// | exr, hdr, dpx, jp2, j2k | oiiotool |
/// | All others | ffmpeg CLI |
async fn render_image_ffmpeg_fallback(
    cook: &mut dyn RenderCook,
    ext: &str,
) -> bool {
    let report = crate::env_check::cached_report();

    // Pick the tool for this extension.
    let use_oiiotool = is_oiio_format(ext);
    let (tool_name, tool_available): (&str, bool) = if use_oiiotool {
        ("oiiotool", report.as_ref()
            .and_then(|r| r.backends.get("oiiotool"))
            .map(|b| b.available).unwrap_or(false))
    } else {
        ("ffmpeg_cli", report.as_ref()
            .and_then(|r| r.backends.get("ffmpeg_cli"))
            .map(|b| b.available).unwrap_or(false))
    };

    if !tool_available { return false; }

    let Some(mut reader) = cook.take_reader() else { return false; };
    let ext_owned = ext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        use std::io::Read;
        reader.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;

        if use_oiiotool {
            run_oiiotool_decode(&buf, &ext_owned)
        } else {
            run_ffmpeg_decode(&buf, &ext_owned, false)
        }
    }).await;

    match result {
        Ok(Ok(out)) => { apply_render_output(cook, out); true }
        Ok(Err(msg)) => { cook.fail_cook(&format!("{tool_name}: {msg}")); true }
        Err(_) => { cook.fail_cook(&format!("{tool_name} panicked")); true }
    }
}

/// Tier-3 ffmpeg CLI fallback for video.  Reads only the first 10 MiB —
/// ffmpeg can often extract a keyframe from a truncated file.
async fn render_video_ffmpeg_fallback(
    cook: &mut dyn RenderCook,
    ext: &str,
) -> bool {
    let report = crate::env_check::cached_report();
    let has_ffmpeg = report.as_ref()
        .and_then(|r| r.backends.get("ffmpeg_cli"))
        .map(|b| b.available).unwrap_or(false);
    if !has_ffmpeg { return false; }

    let Some(mut reader) = cook.take_reader() else { return false; };
    let ext_owned = ext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::with_capacity(10 * 1024 * 1024);
        use std::io::Read;
        // Read at most 10 MiB — ffmpeg works with truncated files.
        let mut chunk = [0u8; 8192];
        loop {
            let n = reader.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
            if n == 0 { break; }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() >= 10 * 1024 * 1024 { break; }
        }
        run_ffmpeg_decode(&buf, &ext_owned, true)
    }).await;

    match result {
        Ok(Ok(out)) => { apply_render_output(cook, out); true }
        Ok(Err(msg)) => { cook.fail_cook(&msg); true }
        Err(_) => { cook.fail_cook("ffmpeg_cli panicked"); true }
    }
}
/// Run OpenImageIO `oiiotool` to decode a studio-format image to PNG, with
/// properties via `--info` and power-of-2 downscaling.  Handles EXR, HDR,
/// DPX, and other formats that ffmpeg/magick may struggle with.
fn run_oiiotool_decode(
    bytes: &[u8],
    ext: &str,
) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(100 * 1024 * 1024)
        .map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena.stage_bytes(bytes, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    // ── oiiotool --info: get dimensions ────────────────────────────────────
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

    // ── power-of-2 downscale ──────────────────────────────────────────────
    let mut resize_w = src_w;
    let mut resize_h = src_h;
    let max_dim = src_w.max(src_h);
    if max_dim > 512 {
        let mut s = 1u32;
        while max_dim / (s * 2) >= 256 { s *= 2; }
        resize_w = src_w / s;
        resize_h = src_h / s;
    }

    // ── oiiotool: decode + resize → PNG ───────────────────────────────────
    let output_path = arena.output_path("png");
    let mut cmd = Command::new("oiiotool");
    cmd.arg(&input_path);
    if resize_w != src_w || resize_h != src_h {
        cmd.arg("--resize").arg(format!("{resize_w}x{resize_h}"));
    }
    cmd.arg("-o").arg(&output_path)
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::piped());
    // No sandbox — oiiotool is a trusted system tool.

    let output = cmd.output().map_err(|e| format!("spawn oiiotool: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("(no output)");
        return Err(format!("oiiotool: {first_line}"));
    }

    let png_bytes = arena.read_output(&output_path)
        .map_err(|e| format!("read output: {e}"))?;
    let img = image::load_from_memory(&png_bytes)
        .map_err(|e| format!("decode PNG output: {e}"))?;

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
async fn render_document_tier3(
    cook: &mut dyn RenderCook,
    ext: &str,
) -> bool {
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
async fn render_geometry_tier3(
    cook: &mut dyn RenderCook,
    ext: &str,
) -> bool {
    // Find the first registered handler that claims this extension.
    let handlers = crate::env_check::registered_handlers();
    let Some(handler) = handlers.iter().find(|h| h.extensions.contains(&ext)) else {
        return false;
    };

    // Check that the handler passed its availability probe.
    let report = crate::env_check::cached_report();
    let available = report.as_ref()
        .and_then(|r| r.backends.get(handler.name))
        .map(|b| b.available)
        .unwrap_or(false);

    if !available {
        return false;
    }

    // Try the subprocess backend.
    if let Some(mut reader) = cook.take_reader() {
        let cmd = handler.command.to_string();
        let name = handler.name.to_string();
        let ext_owned = ext.to_string();
        let result = tokio::task::spawn_blocking(move || {
            run_subprocess_handler(&mut *reader, &ext_owned, &cmd)
        }).await;

        match result {
            Ok(Ok(out)) => {
                apply_render_output(cook, out);
                return true;
            }
            Ok(Err(msg)) => {
                // Renderer ran but failed — log the message, fall through
                // to placeholder.
                eprintln!("[tier3] {name}: {msg}");
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

    // Create a scratch arena for this invocation.
    let arena = ScratchArena::new(100 * 1024 * 1024) // 100 MiB limit
        .map_err(|e| format!("scratch arena: {e}"))?;

    // Stage the source to a temp file.
    let input_path = arena.stage_reader(reader, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    // Allocate output paths.
    let output_path = arena.output_path("jpg");
    let props_path  = arena.output_path("json");

    // Run the renderer in a sandboxed subprocess.
    let mut cmd = Command::new(command);
    cmd.arg(&input_path)
       .arg(&output_path)
       .arg(&props_path)
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::piped());

    crate::sandbox::apply(&mut cmd, &crate::sandbox::default_strict());

    let status = cmd.status()
        .map_err(|e| format!("spawn {command}: {e}"))?;

    if !status.success() {
        return Err(format!("exited with {status}"));
    }

    // Read back the rendered image.
    let jpeg_bytes = arena.read_output(&output_path)
        .map_err(|e| format!("read output: {e}"))?;

    // Decode the JPEG output.
    let img = image::load_from_memory(&jpeg_bytes)
        .map_err(|e| format!("decode output JPEG: {e}"))?;

    // Read the renderer-produced properties.
    let props = std::fs::read_to_string(&props_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());

    // Arena is dropped here — temp files cleaned up.

    Ok(RenderOutput {
        image:           img,
        renderer:        Some(command.to_string()),
        codec:           None,
        video_seek_secs: None,
        properties:      props,
    })
}
