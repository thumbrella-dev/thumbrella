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

// Force the linker to include img2dec.o from libavformat.a.  LLD's
// --gc-sections removes unreferenced data sections, so every demuxer struct
// that must be reachable via av_find_input_format() needs an explicit
// read_volatile reference.  All these symbols live in img2dec.o.
unsafe extern "C" {
    static ff_image_webp_pipe_demuxer:  u8;
    static ff_image_png_pipe_demuxer:   u8;
    static ff_image_jpeg_pipe_demuxer:  u8;
    static ff_image_bmp_pipe_demuxer:   u8;
    static ff_image_tiff_pipe_demuxer:  u8;
    static ff_image_psd_pipe_demuxer:   u8;
    // ico demuxer lives in icodeac.o
    static ff_ico_demuxer:              u8;
}

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
    read_calls: u64,
    read_bytes: u64,
    read_zero: u64,
    read_short: u64,
    read_errors: u64,
    seek_calls: u64,
    seek_errors: u64,
    last_seek_offset: i64,
    last_seek_whence: c_int,
}

use ffmpeg_sys_next::*;
use image::{DynamicImage, RgbImage, RgbaImage};
use serde_json::json;

use tier1::renderer::RenderOutput;
use tier1::spec::ThumbnailConfig;

/// Emit a debug message only when raw logs are enabled (TBR_LOG=full).
/// avdecode cannot use tier1::ux (native-only), so it checks the env var directly.
macro_rules! tbr_debug {
    ($($arg:tt)*) => {
        if matches!(std::env::var("TBR_LOG").as_deref(), Ok("full")) {
            eprintln!($($arg)*);
        }
    };
}

// ── Orientation helpers ───────────────────────────────────────────────────────

/// Rotate a `DynamicImage` by a clockwise angle (0 / 90 / 180 / 270 degrees).
/// Angles that are not a multiple of 90 are treated as 0 (no rotation).
fn apply_rotation(img: DynamicImage, clockwise_degrees: i32) -> DynamicImage {
    match clockwise_degrees.rem_euclid(360) {
        90  => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _   => img,
    }
}

unsafe fn tile_grid_dimensions(fmt_ctx: *mut AVFormatContext) -> Option<(i32, i32)> {
    let group_count = (*fmt_ctx).nb_stream_groups as usize;
    let mut best: Option<(i32, i32)> = None;

    for i in 0..group_count {
        let group = *(*fmt_ctx).stream_groups.add(i);
        if (*group).type_ != AVStreamGroupParamsType::AV_STREAM_GROUP_PARAMS_TILE_GRID {
            continue;
        }

        let tile_grid = unsafe { (*group).params.tile_grid };
        if tile_grid.is_null() {
            continue;
        }

        let width = if (*tile_grid).width > 0 {
            (*tile_grid).width
        } else {
            (*tile_grid).coded_width
        };
        let height = if (*tile_grid).height > 0 {
            (*tile_grid).height
        } else {
            (*tile_grid).coded_height
        };

        if width <= 0 || height <= 0 {
            continue;
        }

        let area = width as i64 * height as i64;
        let keep = best
            .map(|(best_w, best_h)| area > best_w as i64 * best_h as i64)
            .unwrap_or(true);
        if keep {
            best = Some((width, height));
        }
    }

    best
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
    state.read_calls += 1;
    let slice = std::slice::from_raw_parts_mut(buf, buf_size as usize);
    match state.reader.read(slice) {
        Ok(0) => {
            state.read_zero += 1;
            AVERROR_EOF
        }
        Ok(n) => {
            state.read_bytes += n as u64;
            if n < buf_size as usize {
                state.read_short += 1;
            }
            n as c_int
        }
        Err(e) => {
            state.read_errors += 1;
            if state.read_errors <= 4 {
                tbr_debug!("[avdecode][avio] read error {}: {}", state.read_errors, e);
            }
            -5 // −EIO
        }
    }
}

/// Seek the reader.
///
/// `whence` is one of `SEEK_SET`, `SEEK_CUR`, `SEEK_END`, or the libav
/// pseudo-flag `AVSEEK_SIZE` (return total length without seeking).
unsafe extern "C" fn avio_seek_cb(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let state = &mut *(opaque as *mut ReaderState);
    state.seek_calls += 1;
    state.last_seek_offset = offset;
    state.last_seek_whence = whence;
    match whence {
        SEEK_SET => state.reader.seek(SeekFrom::Start(offset as u64)).map(|p| p as i64).unwrap_or_else(|_| {
            state.seek_errors += 1;
            -1
        }),
        SEEK_CUR => state.reader.seek(SeekFrom::Current(offset)).map(|p| p as i64).unwrap_or_else(|_| {
            state.seek_errors += 1;
            -1
        }),
        SEEK_END => state.reader.seek(SeekFrom::End(offset)).map(|p| p as i64).unwrap_or_else(|_| {
            state.seek_errors += 1;
            -1
        }),
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
        "jpeg" | "jpg"          => Some("jpeg_pipe"),
        "png"                   => Some("png_pipe"),
        "bmp"                   => Some("bmp_pipe"),
        "gif"                   => Some("gif"),
        "tiff" | "tif"          => Some("tiff_pipe"),
        "ico"                   => Some("ico"),
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
/// Returns `(result, avio_bytes)` where `avio_bytes` is the total number of
/// bytes requested from the reader by libav's AVIO callbacks.  This value is
/// more accurate than `content_length` for formats that use partial reads
/// (e.g. HEIC/AVIF with probe_limit).
pub fn decode_with_libav(
    reader: Box<dyn ReadSeek + Send>,
    content_length: Option<u64>,
    ext_hint: Option<String>,
    rotation_hint: i32,
    seek_secs: Option<f64>,
) -> (Option<RenderOutput>, u64) {
    tbr_debug!("[avdecode] decode_with_libav: content_length={content_length:?}, ext_hint={ext_hint:?}, rotation_hint={rotation_hint}, seek_secs={seek_secs:?}");

    // Force-retain all pipe demuxer structs from img2dec.o and icodeac.o.
    // LLD's --gc-sections removes data sections that aren't reachable from a
    // live root; read_volatile prevents the optimizer from removing the refs.
    unsafe {
        let _ = std::ptr::read_volatile(&raw const ff_image_webp_pipe_demuxer);
        let _ = std::ptr::read_volatile(&raw const ff_image_png_pipe_demuxer);
        let _ = std::ptr::read_volatile(&raw const ff_image_jpeg_pipe_demuxer);
        let _ = std::ptr::read_volatile(&raw const ff_image_bmp_pipe_demuxer);
        let _ = std::ptr::read_volatile(&raw const ff_image_tiff_pipe_demuxer);
        let _ = std::ptr::read_volatile(&raw const ff_image_psd_pipe_demuxer);
        let _ = std::ptr::read_volatile(&raw const ff_ico_demuxer);
    }

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
        tbr_debug!("[avdecode] using format hint for {:?}", ext_hint);
    }

    // Suppress ffmpeg's own stderr output unless TBR_LOG=full.
    // libav prints warnings and info messages directly to stderr through
    // its internal logging — not through our eprintln! calls.
    let ff_log_level = if matches!(std::env::var("TBR_LOG").as_deref(), Ok("full")) {
        ffmpeg_sys_next::AV_LOG_INFO
    } else {
        ffmpeg_sys_next::AV_LOG_QUIET
    };
    unsafe { ffmpeg_sys_next::av_log_set_level(ff_log_level); }

    // Box the reader state so it has a stable address.  All libav callbacks
    // hold a raw pointer into this box; the box must outlive every libav
    // resource.
    let mut state = Box::new(ReaderState {
        reader,
        content_length,
        read_calls: 0,
        read_bytes: 0,
        read_zero: 0,
        read_short: 0,
        read_errors: 0,
        seek_calls: 0,
        seek_errors: 0,
        last_seek_offset: 0,
        last_seek_whence: 0,
    });
    let opaque = state.as_mut() as *mut ReaderState as *mut c_void;

    // Null-initialise all resource handles so the cleanup block can safely
    // check and free only the ones that were actually allocated.
    let mut avio_ctx:  *mut AVIOContext    = ptr::null_mut();
    let mut fmt_ctx:   *mut AVFormatContext = ptr::null_mut();
    let mut codec_ctx: *mut AVCodecContext  = ptr::null_mut();
    let mut packet:    *mut AVPacket        = ptr::null_mut();
    let mut frame:     *mut AVFrame         = ptr::null_mut();
    let mut sws_ctx:   *mut SwsContext      = ptr::null_mut();

    // Limit probe reads for container formats.
    // HEIF-family: all codec parameters in the `meta` box at file start.
    // Video containers: 256 KB is enough to find stream info + first keyframe
    // without chasing MKV's end-of-file Cues or reading the full mdat.
    let probe_limit: Option<i64> = match ext_hint.as_deref() {
        Some("heic" | "heif" | "heics" | "heifs" | "avif") => Some(128 * 1024),
        _ => Some(256 * 1024),
    };

    let result = unsafe {
        decode_inner(
            opaque,
            fmt_hint_ptr,
            rotation_hint,
            seek_secs,
            probe_limit,
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
    tbr_debug!("[avdecode][avio] reads={} bytes={} short_reads={} eof_reads={} read_errors={} seeks={} seek_errors={} last_seek=(off={}, whence={})",
        state.read_calls,
        state.read_bytes,
        state.read_short,
        state.read_zero,
        state.read_errors,
        state.seek_calls,
        state.seek_errors,
        state.last_seek_offset,
        state.last_seek_whence,
    );

    // state drops here, after every libav resource that held its pointer is gone.
    let avio_bytes = state.read_bytes;
    drop(state);

    (result, avio_bytes)
}

// ── Inner decode — returns early on any failure ───────────────────────────────

#[allow(clippy::too_many_arguments)]
unsafe fn decode_inner(
    opaque:        *mut c_void,
    fmt_hint:      *mut AVInputFormat,
    rotation_hint: i32,
    seek_secs:     Option<f64>,
    // When Some(n): cap avformat_find_stream_info to n bytes.  HEIF/HEIC/AVIF
    // store all codec parameters in the meta box at the file start (<5 KB),
    // so a 128 KB ceiling avoids streaming the full multi-MB mdat block.
    probe_limit:   Option<i64>,
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
        tbr_debug!("[avdecode] FAIL: av_malloc returned null");
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
        tbr_debug!("[avdecode] FAIL: avio_alloc_context returned null");
        av_free(avio_buf as *mut c_void);
        return None;
    }

    // ── AVFormatContext ───────────────────────────────────────────────────────
    *fmt_ctx = avformat_alloc_context();
    if (*fmt_ctx).is_null() {
        tbr_debug!("[avdecode] FAIL: avformat_alloc_context returned null");
        return None;
    }
    (**fmt_ctx).pb    = *avio_ctx;
    (**fmt_ctx).flags |= AVFMT_FLAG_CUSTOM_IO as i32;
    // Prevent MKV/WebM from reading the end-of-file Cues index.
    // We only need the first keyframe near the seek point.
    (**fmt_ctx).flags |= AVFMT_FLAG_NOFILLIN as i32;

    // avformat_open_input frees *fmt_ctx on failure; null it to avoid
    // a double-free in the caller's cleanup.
    let open_ret = avformat_open_input(fmt_ctx, ptr::null(), fmt_hint, ptr::null_mut());
    if open_ret < 0 {
        tbr_debug!("[avdecode] FAIL: avformat_open_input returned {open_ret}");
        *fmt_ctx = ptr::null_mut();
        return None;
    }
    tbr_debug!("[avdecode] avformat_open_input OK");

    // Apply probe limit BEFORE find_stream_info so FFmpeg stops reading after
    // at most `limit` bytes.  For HEIF/HEIC/AVIF the codec parameters are
    // entirely in the meta box (first ~5 KB); limiting to 128 KB is safe and
    // reduces HEIC downloads from ~2.7 MB to ~30 KB.
    if let Some(limit) = probe_limit {
        (**fmt_ctx).probesize = limit;
        (**fmt_ctx).max_analyze_duration = 0;
    }

    let info_ret = avformat_find_stream_info(*fmt_ctx, ptr::null_mut());
    if info_ret < 0 {
        tbr_debug!("[avdecode] FAIL: avformat_find_stream_info returned {info_ret}");
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
    tbr_debug!("[avdecode] {} stream(s) found", nb);

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
        tbr_debug!("[avdecode] FAIL: no video/image stream found among {nb} streams");
        return None;
    }
    let (standalone_flag, best_area) = best_score;
    tbr_debug!("[avdecode] using stream {stream_idx} (area={best_area} standalone={standalone_flag})");


    let stream    = *(**fmt_ctx).streams.add(stream_idx as usize);
    let codecpar  = (*stream).codecpar;
    let src_w     = (*codecpar).width;
    let src_h     = (*codecpar).height;
    let pix_fmt: AVPixelFormat = std::mem::transmute((*codecpar).format);
    let codec_id  = (*codecpar).codec_id;
    let (reported_w, reported_h) = tile_grid_dimensions(*fmt_ctx).unwrap_or((src_w, src_h));

    // Codec name for the trace record.
    let codec_name: Option<String> = {
        let p = avcodec_get_name(codec_id);
        if p.is_null() {
            None
        } else {
            Some(CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    };
    tbr_debug!("[avdecode] codec={codec_name:?}  src={src_w}x{src_h} reported={reported_w}x{reported_h}");

    // Bits-per-pixel from the pixel-format descriptor.
    let depth = {
        let desc = av_pix_fmt_desc_get(pix_fmt);
        if desc.is_null() { 0 } else { av_get_bits_per_pixel(desc) }
    };

    // ── Decoder ───────────────────────────────────────────────────────────────
    let dec = avcodec_find_decoder(codec_id);
    if dec.is_null() {
        tbr_debug!("[avdecode] FAIL: no decoder for codec_id {codec_id:?}");
        return None;
    }
    *codec_ctx = avcodec_alloc_context3(dec);
    if (*codec_ctx).is_null() {
        tbr_debug!("[avdecode] FAIL: avcodec_alloc_context3 returned null");
        return None;
    }
    if avcodec_parameters_to_context(*codec_ctx, codecpar) < 0 {
        tbr_debug!("[avdecode] FAIL: avcodec_parameters_to_context failed");
        return None;
    }
    // Auto-detect thread count (libdav1d etc. respect this).
    // 0 = auto; decoder chooses based on available cores.
    unsafe { (*(*codec_ctx)).thread_count = 0; }
    let open2_ret = avcodec_open2(*codec_ctx, dec, ptr::null_mut());
    if open2_ret < 0 {
        tbr_debug!("[avdecode] FAIL: avcodec_open2 returned {open2_ret}");
        return None;
    }

    // Video thumbnail path: try a lightweight seek to ~1s before decoding.
    // This approximates ffmpeg `-ss 1` before input and avoids many first-frame
    // black/splash thumbnails.
    let mut applied_seek_secs: Option<f64> = None;
    if let Some(secs) = seek_secs.filter(|s| *s > 0.0) {
        let time_base = (*stream).time_base;
        if time_base.num > 0 && time_base.den > 0 {
            let target_ts = ((secs * time_base.den as f64) / time_base.num as f64).round() as i64;
            let seek_ret = av_seek_frame(*fmt_ctx, stream_idx, target_ts, AVSEEK_FLAG_BACKWARD as c_int);
            if seek_ret >= 0 {
                avcodec_flush_buffers(*codec_ctx);
                applied_seek_secs = Some(secs);
                tbr_debug!("[avdecode] seek applied: {secs:.3}s (ts={target_ts})");
            } else {
                tbr_debug!("[avdecode] seek skipped: av_seek_frame returned {seek_ret}");
            }
        }
    }

    // ── Decode first frame ────────────────────────────────────────────────────
    *packet = av_packet_alloc();
    *frame  = av_frame_alloc();
    if (*packet).is_null() || (*frame).is_null() {
        tbr_debug!("[avdecode] FAIL: av_packet_alloc or av_frame_alloc returned null");
        return None;
    }

    let mut decoded = false;
    let attempts = if applied_seek_secs.is_some() { 2 } else { 1 };
    for attempt in 0..attempts {
        if attempt == 1 {
            // Seeked decode failed once; retry from stream start for robustness.
            let _ = av_seek_frame(*fmt_ctx, stream_idx, 0, AVSEEK_FLAG_BACKWARD as c_int);
            avcodec_flush_buffers(*codec_ctx);
            tbr_debug!("[avdecode] retrying decode from stream start");
        }

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

        // Flush the decoder - some codecs buffer the frame and only emit it
        // when EOF is signalled by a NULL packet.
        if !decoded {
            if avcodec_send_packet(*codec_ctx, ptr::null_mut()) >= 0 {
                if avcodec_receive_frame(*codec_ctx, *frame) >= 0 {
                    decoded = true;
                }
            }
        }

        if decoded {
            break;
        }
    }
    if !decoded {
        tbr_debug!("[avdecode] FAIL: could not decode a frame");
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
    tbr_debug!("[avdecode] decoded frame {frame_w}x{frame_h}  rotation={rotation_degrees}°");

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

    // Use RGBA output when the source pixel format carries an alpha channel,
    // RGB24 otherwise.  sws_scale handles the conversion in both cases.
    let has_alpha = {
        let desc = av_pix_fmt_desc_get(frame_fmt);
        !desc.is_null() && ((*desc).flags & AV_PIX_FMT_FLAG_ALPHA as u64) != 0
    };
    let dst_fmt  = if has_alpha { AVPixelFormat::AV_PIX_FMT_RGBA } else { AVPixelFormat::AV_PIX_FMT_RGB24 };
    let channels = if has_alpha { 4usize } else { 3usize };

    *sws_ctx = sws_getContext(
        frame_w, frame_h, frame_fmt,
        out_w,   out_h,   dst_fmt,
        SWS_BILINEAR as c_int,
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null(),
    );
    if (*sws_ctx).is_null() {
        tbr_debug!("[avdecode] FAIL: sws_getContext returned null");
        return None;
    }

    let stride = (out_w as usize) * channels;
    let mut buf = vec![0u8; stride * out_h as usize];
    let dst_data:     [*mut u8; 4] = [buf.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
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
    let img: DynamicImage = if has_alpha {
        match RgbaImage::from_raw(out_w as u32, out_h as u32, buf) {
            Some(i) => DynamicImage::ImageRgba8(i),
            None => {
                tbr_debug!("[avdecode] FAIL: RgbaImage::from_raw failed (buffer size mismatch)");
                return None;
            }
        }
    } else {
        match RgbImage::from_raw(out_w as u32, out_h as u32, buf) {
            Some(i) => DynamicImage::ImageRgb8(i),
            None => {
                tbr_debug!("[avdecode] FAIL: RgbImage::from_raw failed (buffer size mismatch)");
                return None;
            }
        }
    };
    let img = apply_rotation(img, rotation_degrees);
    tbr_debug!("[avdecode] success: {src_w}x{src_h} codec={codec_name:?} depth={depth} rotation={rotation_degrees}°");
    Some(RenderOutput {
        image:           img,
        renderer:        Some("ffmpeg".to_string()),
        codec:           codec_name,
        video_seek_secs: applied_seek_secs,
        properties:      Some(json!({
            "width_pixels":  reported_w,
            "height_pixels": reported_h,
            "bits_per_pixel":  depth,
        })),
    })
}
