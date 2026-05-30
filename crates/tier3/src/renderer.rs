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
                        // Handoff: lower tier already tried standard decode.
                        // Don't retry — fall through to placeholder.
                        false
                    } else {
                        self.tier2.render(cook).await
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
/// Tier 2 handles standard Huffman-coded JPEGs via libav.  Tier 3
/// intercepts only arithmetic-coded JPEGs (SOF9), which libav's mjpeg
/// decoder does not support, and uses ffmpeg CLI instead.
async fn render_image_tier3(
    cook: &mut dyn RenderCook,
    ext: &str,
) -> bool {
    // Check for arithmetic coding without consuming the reader.
    if !is_arithmetic_jpeg_peek(cook) {
        return false; // let the caller delegate to tier2
    }

    eprintln!("[tier3] arithmetic JPEG detected, using ffmpeg CLI");

    let report = crate::env_check::cached_report();
    let has_ffmpeg_cli = report.as_ref()
        .and_then(|r| r.backends.get("ffmpeg_cli"))
        .map(|b| b.available)
        .unwrap_or(false);

    if !has_ffmpeg_cli {
        return false;
    }

    let Some(mut reader) = cook.take_reader() else {
        return false;
    };

    let ext_owned = ext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        run_ffmpeg_image_decode(&mut *reader, &ext_owned)
    }).await;

    match result {
        Ok(Ok(out)) => {
            apply_render_output(cook, out);
            true
        }
        Ok(Err(msg)) => {
            cook.fail_cook(&format!("ffmpeg_cli: {msg}"));
            true
        }
        Err(_) => {
            cook.fail_cook("ffmpeg_cli panicked");
            true
        }
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

/// Run ffmpeg CLI to decode an image to PNG, then load it.
///
/// `ffmpeg -i input.ext output.png -y` handles every JPEG variant including
/// arithmetic coding (SOF9), which libav's mjpeg decoder rejects.
fn run_ffmpeg_image_decode(
    reader: &mut dyn tier1::ReadSeek,
    ext: &str,
) -> Result<RenderOutput, String> {
    use std::process::Command;

    let arena = ScratchArena::new(50 * 1024 * 1024) // 50 MiB — images are small
        .map_err(|e| format!("scratch arena: {e}"))?;

    let input_path = arena.stage_reader(reader, &format!("input.{ext}"))
        .map_err(|e| format!("stage input: {e}"))?;

    let output_path = arena.output_path("png");

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-i").arg(&input_path)
       .arg(&output_path)
       .arg("-y")
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::piped());

    crate::sandbox::apply(&mut cmd, &crate::sandbox::default_strict());

    let status = cmd.status()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;

    if !status.success() {
        return Err(format!("ffmpeg exited with {status}"));
    }

    let png_bytes = arena.read_output(&output_path)
        .map_err(|e| format!("read output: {e}"))?;

    let img = image::load_from_memory(&png_bytes)
        .map_err(|e| format!("decode PNG output: {e}"))?;

    let (src_w, src_h) = image::GenericImageView::dimensions(&img);
    let depth = img.color().bits_per_pixel();

    Ok(RenderOutput {
        image:           img,
        renderer:        Some("ffmpeg_cli".into()),
        codec:           None,
        video_seek_secs: None,
        properties:      Some(serde_json::json!({
            "width": src_w,
            "height": src_h,
            "depth": depth,
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
