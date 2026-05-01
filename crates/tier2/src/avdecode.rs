//! libav image decode — custom AVIOContext backed by any `Read + Seek` source.
//!
//! # Design
//!
//! libav performs all I/O through a caller-supplied [`AVIOContext`].  The
//! two C callbacks (`avio_read_cb`, `avio_seek_cb`) delegate to a
//! [`ReaderState`] that holds a type-erased `Box<dyn Read + Seek + Send>`.
//!
//! Callers can supply:
//!
//! * `Box::new(std::io::Cursor::new(bytes))` — reads from a pre-fetched
//!   `Vec<u8>` already held in memory.
//! * `Box::new(SyncHttpReader::new(http_buf))` — reads from the live
//!   `HttpBuffer` paged cache.  Each callback invocation calls
//!   `handle.block_on(async_read)` inside the `spawn_blocking` thread, so
//!   libav drives the HTTP download on-demand without buffering the whole
//!   file upfront.  This is the preferred path for large video files.
//!
//! The entry point is [`decode_with_libav`].  It is synchronous and
//! CPU-bound; callers should invoke it from `tokio::task::spawn_blocking`.

// This module is entirely FFI glue.  Every unsafe fn here has an inherently
// unsafe body because it's calling raw C library functions with raw pointers.
// Requiring extra `unsafe {}` blocks inside `unsafe fn` bodies adds noise with
// no safety benefit for code of this nature.
#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::{CStr, CString};
use std::io::{Read, Seek, SeekFrom};
use std::os::raw::{c_int, c_void};
use std::ptr;

use tier1::ReadSeek;

// ── ReaderState — type-erased I/O state for AVIO callbacks ───────────────────

/// Holds the reader and its optional total length.
///
/// A raw pointer to this struct is stored in `AVIOContext::opaque`.
/// The `opaque` pointer is only ever accessed from the two callbacks below,
/// which run synchronously on the same thread as [`decode_with_libav`].
struct ReaderState {
    reader: Box<dyn ReadSeek + Send>,
    /// `Content-Length` or equivalent total size, used to answer `AVSEEK_SIZE`.
    content_length: Option<u64>,
}

use ffmpeg_sys_next::*;
use image::{DynamicImage, RgbImage};
use serde_json::json;

use tier1::renderer::RenderOutput;
use tier1::spec::ThumbnailConfig;

// ── Orientation helpers ───────────────────────────────────────────────────────

/// Rotate a `DynamicImage` by a clockwise angle (0 / 90 / 180 / 270 degrees).
/// Angles that are not a multiple of 90 are treated as 0 (no rotation).
fn apply_rotation(img: DynamicImage, clockwise_degrees: i32) -> DynamicImage {
    match clockwise_degrees.rem_euclid(360) {
        90  => DynamicImage::ImageRgb8(img.rotate90().into_rgb8()),
        180 => DynamicImage::ImageRgb8(img.rotate180().into_rgb8()),
        270 => DynamicImage::ImageRgb8(img.rotate270().into_rgb8()),
        _   => img,
    }
}

// ── Seek whence constants (POSIX) ─────────────────────────────────────────────

const SEEK_SET: c_int = 0;
const SEEK_CUR: c_int = 1;
const SEEK_END: c_int = 2;
/// libav-specific: return the total stream size (no actual seek).
const AVSEEK_SIZE: c_int = 0x10000;

// ── AVIOContext callbacks ─────────────────────────────────────────────────────

/// Read `buf_size` bytes from the reader into `buf`.
///
/// Returns the number of bytes read, or `AVERROR_EOF` on end-of-stream.
/// The opaque pointer is `*mut ReaderState`.
unsafe extern "C" fn avio_read_cb(opaque: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let state = &mut *(opaque as *mut ReaderState);
    let slice = std::slice::from_raw_parts_mut(buf, buf_size as usize);
    match state.reader.read(slice) {
        Ok(0) => AVERROR_EOF,
        Ok(n) => n as c_int,
        Err(_) => -5, // −EIO
    }
}

/// Seek the reader.
///
/// `whence` is one of `SEEK_SET`, `SEEK_CUR`, `SEEK_END`, or the libav
/// pseudo-flag `AVSEEK_SIZE` (return total length without seeking).
unsafe extern "C" fn avio_seek_cb(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let state = &mut *(opaque as *mut ReaderState);
    match whence {
        SEEK_SET => state.reader.seek(SeekFrom::Start(offset as u64)).map(|p| p as i64).unwrap_or(-1),
        SEEK_CUR => state.reader.seek(SeekFrom::Current(offset)).map(|p| p as i64).unwrap_or(-1),
        SEEK_END => state.reader.seek(SeekFrom::End(offset)).map(|p| p as i64).unwrap_or(-1),
        w if w == AVSEEK_SIZE => state.content_length.map(|n| n as i64).unwrap_or(-1),
        _ => -1,
    }
}

// ── Resource cleanup helpers ──────────────────────────────────────────────────

/// Free an `AVIOContext` that was created with `avio_alloc_context`.
///
/// The internal buffer must be freed separately before calling
/// `avio_context_free` because for custom-IO contexts FFmpeg does not
/// own the buffer.
unsafe fn free_avio_ctx(ctx: &mut *mut AVIOContext) {
    if (*ctx).is_null() {
        return;
    }
    let buf = (**ctx).buffer;
    if !buf.is_null() {
        av_free(buf as *mut c_void);
        (**ctx).buffer = ptr::null_mut();
    }
    avio_context_free(ctx);
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Map a canonical file extension to the libav input format name to use as a
/// probe hint when `avformat_open_input` cannot identify the format from the
/// byte stream alone (e.g. raw JPEG/PNG piped through a custom AVIOContext
/// without a filename).
fn ext_to_libav_format(ext: &str) -> Option<&'static str> {
    match ext {
        "jpeg" | "jpg"          => Some("mjpeg"),
        "png"                   => Some("png_pipe"),
        "bmp"                   => Some("bmp_pipe"),
        "gif"                   => Some("gif"),
        "tiff"                  => Some("tiff_pipe"),
        "webp"                  => Some("webp_pipe"),
        _                       => None,
    }
}

/// Decode a media stream with libav, scale the first video/image frame to the
/// canonical thumbnail output size (RGB24), and return a [`RenderOutput`].
///
/// # Parameters
///
/// * `reader` — any `Read + Seek + Send` source.  Pass
///   `Box::new(std::io::Cursor::new(bytes))` for in-memory bytes, or
///   `Box::new(SyncHttpReader::new(http_buf))` to let libav drive the HTTP
///   download on-demand through our paged cache.
/// * `content_length` — total byte size of the stream, if known.  Used to
///   answer libav's `AVSEEK_SIZE` probe; pass `None` if unknown.
/// * `ext_hint` — canonical file extension (e.g. `"heic"`, `"avif"`), used
///   to hint the demuxer when probing an untitled stream.  Pass `None` to
///   rely entirely on byte-stream detection.
///
/// Returns `None` on any failure (unsupported format, decode error, etc.).
pub fn decode_with_libav(
    reader: Box<dyn ReadSeek + Send>,
    content_length: Option<u64>,
    ext_hint: Option<String>,
    rotation_hint: i32,
) -> Option<RenderOutput> {
    eprintln!("[avdecode] decode_with_libav: content_length={content_length:?}, ext_hint={ext_hint:?}, rotation_hint={rotation_hint}");

    // Resolve the optional format hint to a *mut AVInputFormat.
    // av_find_input_format returns NULL when the name is unknown — that's fine,
    // the NULL will be passed to avformat_open_input (no hint = auto-probe).
    let fmt_hint_ptr: *mut AVInputFormat = unsafe {
        ext_hint.as_deref()
            .and_then(ext_to_libav_format)
            .and_then(|name| {
                CString::new(name).ok()
                    .map(|cs| av_find_input_format(cs.as_ptr()) as *mut _)
            })
            .unwrap_or(ptr::null_mut())
    };

    if !fmt_hint_ptr.is_null() {
        eprintln!("[avdecode] using format hint for {:?}", ext_hint);
    } else {
        eprintln!("[avdecode] no format hint, relying on auto-probe");
    }

    // Box the reader state so it has a stable address.  All libav callbacks
    // hold a raw pointer into this box; the box must outlive every libav
    // resource.
    let mut state = Box::new(ReaderState { reader, content_length });
    let opaque = state.as_mut() as *mut ReaderState as *mut c_void;

    // Null-initialise all resource handles so the cleanup block can safely
    // check and free only the ones that were actually allocated.
    let mut avio_ctx:  *mut AVIOContext    = ptr::null_mut();
    let mut fmt_ctx:   *mut AVFormatContext = ptr::null_mut();
    let mut codec_ctx: *mut AVCodecContext  = ptr::null_mut();
    let mut packet:    *mut AVPacket        = ptr::null_mut();
    let mut frame:     *mut AVFrame         = ptr::null_mut();
    let mut sws_ctx:   *mut SwsContext      = ptr::null_mut();

    let result = unsafe {
        decode_inner(
            opaque,
            fmt_hint_ptr,
            rotation_hint,
            &mut avio_ctx,
            &mut fmt_ctx,
            &mut codec_ctx,
            &mut packet,
            &mut frame,
            &mut sws_ctx,
        )
    };

    // Always free every resource regardless of success/failure.
    // Order matters: codec resources before format, format before AVIO.
    unsafe {
        if !sws_ctx.is_null() {
            sws_freeContext(sws_ctx);
        }
        if !frame.is_null() {
            av_frame_free(&mut frame);
        }
        if !packet.is_null() {
            av_packet_free(&mut packet);
        }
        if !codec_ctx.is_null() {
            avcodec_free_context(&mut codec_ctx);
        }
        if !fmt_ctx.is_null() {
            // avformat_close_input sets fmt_ctx to NULL after freeing.
            avformat_close_input(&mut fmt_ctx);
        }
        // AVIOContext is NOT freed by avformat_close_input when CUSTOM_IO is
        // set — free it ourselves.
        free_avio_ctx(&mut avio_ctx);
    }

    // state drops here, after every libav resource that held its pointer is gone.
    drop(state);

    result
}

// ── Inner decode — returns early on any failure ───────────────────────────────

#[allow(clippy::too_many_arguments)]
unsafe fn decode_inner(
    opaque:        *mut c_void,
    fmt_hint:      *mut AVInputFormat,
    rotation_hint: i32,
    avio_ctx:      &mut *mut AVIOContext,
    fmt_ctx:       &mut *mut AVFormatContext,
    codec_ctx:     &mut *mut AVCodecContext,
    packet:        &mut *mut AVPacket,
    frame:         &mut *mut AVFrame,
    sws_ctx:       &mut *mut SwsContext,
) -> Option<RenderOutput> {
    const AVIO_BUF: usize = 65536;

    // ── AVIO buffer + context ─────────────────────────────────────────────────
    let avio_buf = av_malloc(AVIO_BUF) as *mut u8;
    if avio_buf.is_null() {
        eprintln!("[avdecode] FAIL: av_malloc returned null");
        return None;
    }
    *avio_ctx = avio_alloc_context(
        avio_buf,
        AVIO_BUF as c_int,
        0, // read-only
        opaque,
        Some(avio_read_cb),
        None, // no write callback
        Some(avio_seek_cb),
    );
    if (*avio_ctx).is_null() {
        eprintln!("[avdecode] FAIL: avio_alloc_context returned null");
        av_free(avio_buf as *mut c_void);
        return None;
    }

    // ── AVFormatContext ───────────────────────────────────────────────────────
    *fmt_ctx = avformat_alloc_context();
    if (*fmt_ctx).is_null() {
        eprintln!("[avdecode] FAIL: avformat_alloc_context returned null");
        return None;
    }
    (**fmt_ctx).pb    = *avio_ctx;
    (**fmt_ctx).flags |= AVFMT_FLAG_CUSTOM_IO as i32;

    // avformat_open_input frees *fmt_ctx on failure; null it to avoid
    // a double-free in the caller's cleanup.
    let open_ret = avformat_open_input(fmt_ctx, ptr::null(), fmt_hint, ptr::null_mut());
    if open_ret < 0 {
        eprintln!("[avdecode] FAIL: avformat_open_input returned {open_ret}");
        *fmt_ctx = ptr::null_mut();
        return None;
    }
    eprintln!("[avdecode] avformat_open_input OK");

    let info_ret = avformat_find_stream_info(*fmt_ctx, ptr::null_mut());
    if info_ret < 0 {
        eprintln!("[avdecode] FAIL: avformat_find_stream_info returned {info_ret}");
        return None;
    }

    // ── Find video / image stream ─────────────────────────────────────────────
    // Pick the best stream using a two-level heuristic:
    //
    // 1. **Standalone vs. grid tile** — HEIC/HEIF grid images expose each tile
    //    as a separate video stream (e.g. 60 streams of 512×512 for a 4K grid).
    //    A standalone representative thumbnail has unique dimensions that appear
    //    only once.  When ≥4 streams share the same (w,h), the "dominant" size
    //    is almost certainly a set of grid tiles; any stream with unique dims is
    //    a whole-image thumbnail and is strongly preferred.
    //
    // 2. **Largest area** — among streams in the same preference tier (both
    //    standalone, or both tile), pick the one with the most pixels.
    //    This selects the primary track for ordinary video / MOV files.
    let nb = (**fmt_ctx).nb_streams as usize;
    eprintln!("[avdecode] {} stream(s) found", nb);

    // Count how many video streams share each (w, h).
    let mut dim_freq: std::collections::HashMap<(i32, i32), usize> =
        std::collections::HashMap::new();
    for i in 0..nb {
        let st = *(**fmt_ctx).streams.add(i);
        if (*(*st).codecpar).codec_type == AVMediaType::AVMEDIA_TYPE_VIDEO {
            let w = (*(*st).codecpar).width;
            let h = (*(*st).codecpar).height;
            *dim_freq.entry((w, h)).or_insert(0) += 1;
        }
    }
    let max_freq = dim_freq.values().copied().max().unwrap_or(0);

    // Score: (is_standalone, area). Tuple comparison naturally prefers
    // standalone (true > false), then largest area within the same tier.
    let mut stream_idx: i32 = -1;
    let mut best_score: (bool, i64) = (false, -1);
    for i in 0..nb {
        let st = *(**fmt_ctx).streams.add(i);
        if (*(*st).codecpar).codec_type == AVMediaType::AVMEDIA_TYPE_VIDEO {
            let w = (*(*st).codecpar).width;
            let h = (*(*st).codecpar).height;
            let freq  = *dim_freq.get(&(w, h)).unwrap_or(&1);
            let area  = w as i64 * h as i64;
            // Only promote as "standalone" when there is a clear dominant
            // repeated-size group (grid tiles); otherwise all streams compete
            // on area alone.
            let is_standalone = max_freq >= 4 && freq == 1;
            let score = (is_standalone, area);
            if score > best_score {
                best_score = score;
                stream_idx = i as i32;
            }
        }
    }
    if stream_idx < 0 {
        eprintln!("[avdecode] FAIL: no video/image stream found among {nb} streams");
        return None;
    }
    let (standalone_flag, best_area) = best_score;
    eprintln!("[avdecode] using stream {stream_idx} (area={best_area} standalone={standalone_flag})");


    let stream    = *(**fmt_ctx).streams.add(stream_idx as usize);
    let codecpar  = (*stream).codecpar;
    let src_w     = (*codecpar).width;
    let src_h     = (*codecpar).height;
    let pix_fmt: AVPixelFormat = std::mem::transmute((*codecpar).format);
    let codec_id  = (*codecpar).codec_id;

    // Codec name for the trace record.
    let codec_name: Option<String> = {
        let p = avcodec_get_name(codec_id);
        if p.is_null() {
            None
        } else {
            Some(CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    };
    eprintln!("[avdecode] codec={codec_name:?}  src={src_w}x{src_h}");

    // Bits-per-pixel from the pixel-format descriptor.
    let depth = {
        let desc = av_pix_fmt_desc_get(pix_fmt);
        if desc.is_null() { 0 } else { av_get_bits_per_pixel(desc) }
    };

    // ── Decoder ───────────────────────────────────────────────────────────────
    let dec = avcodec_find_decoder(codec_id);
    if dec.is_null() {
        eprintln!("[avdecode] FAIL: no decoder for codec_id {codec_id:?}");
        return None;
    }
    *codec_ctx = avcodec_alloc_context3(dec);
    if (*codec_ctx).is_null() {
        eprintln!("[avdecode] FAIL: avcodec_alloc_context3 returned null");
        return None;
    }
    if avcodec_parameters_to_context(*codec_ctx, codecpar) < 0 {
        eprintln!("[avdecode] FAIL: avcodec_parameters_to_context failed");
        return None;
    }
    let open2_ret = avcodec_open2(*codec_ctx, dec, ptr::null_mut());
    if open2_ret < 0 {
        eprintln!("[avdecode] FAIL: avcodec_open2 returned {open2_ret}");
        return None;
    }

    // ── Decode first frame ────────────────────────────────────────────────────
    *packet = av_packet_alloc();
    *frame  = av_frame_alloc();
    if (*packet).is_null() || (*frame).is_null() {
        eprintln!("[avdecode] FAIL: av_packet_alloc or av_frame_alloc returned null");
        return None;
    }

    let mut decoded = false;
    while av_read_frame(*fmt_ctx, *packet) >= 0 {
        if (**packet).stream_index == stream_idx {
            if avcodec_send_packet(*codec_ctx, *packet) >= 0
                && avcodec_receive_frame(*codec_ctx, *frame) >= 0
            {
                decoded = true;
                av_packet_unref(*packet);
                break;
            }
        }
        av_packet_unref(*packet);
    }
    // Flush the decoder — some image codecs (e.g. AV1/AVIF, some PNG paths)
    // buffer the frame and only emit it when EOF is signalled by a NULL packet.
    if !decoded {
        if avcodec_send_packet(*codec_ctx, ptr::null_mut()) >= 0 {
            if avcodec_receive_frame(*codec_ctx, *frame) >= 0 {
                decoded = true;
            }
        }
    }
    if !decoded {
        eprintln!("[avdecode] FAIL: could not decode a frame");
        return None;
    }

    let frame_w  = (**frame).width;
    let frame_h  = (**frame).height;
    let frame_fmt: AVPixelFormat = std::mem::transmute((**frame).format);

    // Read the display-matrix side data to detect rotation (common in HEIC/MOV).
    // The matrix is a 3x3 array of i32 in fixed-point 16.16 (9 × 4 = 36 bytes).
    // We derive the angle by checking elements [0][0] and [1][0].
    // Combine display-matrix rotation (from frame side data) with the
    // caller-supplied hint (e.g. parsed from HEIC irot box).
    let rotation_degrees: i32 = {
        let display_matrix_degrees = {
            let sd = av_frame_get_side_data(*frame, AVFrameSideDataType::AV_FRAME_DATA_DISPLAYMATRIX);
            if !sd.is_null() && (*sd).size as usize >= 36 {
                let data = std::slice::from_raw_parts((*sd).data as *const i32, 9);
                let a = data[0] as f64;
                let c = data[3] as f64;
                let angle = (-c).atan2(a).to_degrees();
                ((angle / 90.0).round() as i32 * 90).rem_euclid(360)
            } else {
                0
            }
        };
        if display_matrix_degrees != 0 { display_matrix_degrees }
        else { rotation_hint.rem_euclid(360) }
    };
    eprintln!("[avdecode] decoded frame {frame_w}x{frame_h}  rotation={rotation_degrees}°");

    // ── Scale to cover the canonical thumbnail size (RGB24) ─────────────────
    // Scale so that both dimensions are ≥ the canonical target while
    // preserving aspect ratio.  deliver's fill_crop() handles the final
    // center-crop; scaling to exactly 250×200 here would squish the image.
    let target_w = ThumbnailConfig::CANONICAL.exact_width  as c_int;
    let target_h = ThumbnailConfig::CANONICAL.exact_height as c_int;
    let (out_w, out_h) = {
        let scale = (target_w as f64 / frame_w as f64)
            .max(target_h as f64 / frame_h as f64);
        let sw = ((frame_w as f64 * scale).round() as c_int).max(1);
        let sh = ((frame_h as f64 * scale).round() as c_int).max(1);
        (sw, sh)
    };

    *sws_ctx = sws_getContext(
        frame_w, frame_h, frame_fmt,
        out_w,   out_h,   AVPixelFormat::AV_PIX_FMT_RGB24,
        SWS_BILINEAR as c_int,
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null(),
    );
    if (*sws_ctx).is_null() {
        eprintln!("[avdecode] FAIL: sws_getContext returned null");
        return None;
    }

    let stride = (out_w as usize) * 3;
    let mut rgb_buf = vec![0u8; stride * out_h as usize];
    let dst_data:     [*mut u8; 4] = [rgb_buf.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
    let dst_linesize: [c_int;   4] = [stride as c_int, 0, 0, 0];

    sws_scale(
        *sws_ctx,
        (**frame).data.as_ptr() as *const *const u8,
        (**frame).linesize.as_ptr(),
        0,
        frame_h,
        dst_data.as_ptr()     as *const *mut u8,
        dst_linesize.as_ptr() as *const c_int,
    );

    // ── Build RenderOutput ────────────────────────────────────────────────────
    let img = match RgbImage::from_raw(out_w as u32, out_h as u32, rgb_buf) {
        Some(i) => i,
        None => {
            eprintln!("[avdecode] FAIL: RgbImage::from_raw failed (buffer size mismatch)");
            return None;
        }
    };
    let img = apply_rotation(DynamicImage::ImageRgb8(img), rotation_degrees);
    eprintln!("[avdecode] success: {src_w}x{src_h} codec={codec_name:?} depth={depth} rotation={rotation_degrees}°");
    Some(RenderOutput {
        image:           img,
        renderer:        Some("ffmpeg".to_string()),
        codec:           codec_name,
        video_seek_secs: None,
        properties:      Some(json!({
            "width":  src_w,
            "height": src_h,
            "depth":  depth,
        })),
    })
}
