#!/usr/bin/env bash
# Generate minimal FFmpeg 7.1 FFI bindings for the current platform.
#
# Requires: bindgen-cli, libclang, FFmpeg headers in $FFMPEG_DIR/include
#
# Usage:
#   FFMPEG_DIR=target/ffmpeg-static ./tier2/src/ffmpeg/generate_bindings.sh
#
# Output: tier2/src/ffmpeg/<platform>.rs  (e.g. linux_x64.rs, windows_x64.rs)

set -euo pipefail

FFMPEG_DIR="${FFMPEG_DIR:-/opt/ffmpeg-static}"
HEADER_DIR="${FFMPEG_DIR}/include"
OUT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Detect platform for output filename.
case "$(rustc -vV | grep host | cut -d' ' -f2)" in
    *linux*)   PLATFORM="linux_x64" ;;
    *windows*) PLATFORM="windows_x64" ;;
    *apple*)   PLATFORM="macos_x64" ;;
    *)         PLATFORM="unknown" ;;
esac
OUTPUT="${OUT_DIR}/${PLATFORM}.rs"

echo "[bindings] Generating FFmpeg bindings for ${PLATFORM}..."
echo "[bindings] FFMPEG_DIR=${FFMPEG_DIR}"
echo "[bindings] Output: ${OUTPUT}"

# Check prerequisites.
if ! command -v bindgen &>/dev/null; then
    echo "ERROR: bindgen not found. Install: cargo install bindgen-cli" >&2
    exit 1
fi

if [[ ! -f "${HEADER_DIR}/libavcodec/avcodec.h" ]]; then
    echo "ERROR: FFmpeg headers not found at ${HEADER_DIR}" >&2
    echo "Run build_static_ffmpeg.sh or set FFMPEG_DIR." >&2
    exit 1
fi

# The 52 symbols we actually use, plus types they transitively need.
# bindgen follows the type graph automatically, so we just list the
# entry points - structs, functions, and constants we reference directly.
bindgen \
    "${HEADER_DIR}/libavcodec/avcodec.h" \
    "${HEADER_DIR}/libavformat/avformat.h" \
    "${HEADER_DIR}/libavformat/avio.h" \
    "${HEADER_DIR}/libavutil/frame.h" \
    "${HEADER_DIR}/libavutil/pixdesc.h" \
    "${HEADER_DIR}/libavutil/log.h" \
    "${HEADER_DIR}/libavutil/channel_layout.h" \
    "${HEADER_DIR}/libswscale/swscale.h" \
    "${HEADER_DIR}/libswresample/swresample.h" \
    \
    --allowlist-function "av_read_frame" \
    --allowlist-function "av_seek_frame" \
    --allowlist-function "avcodec_find_decoder" \
    --allowlist-function "avcodec_open2" \
    --allowlist-function "avcodec_close" \
    --allowlist-function "avcodec_free_context" \
    --allowlist-function "avcodec_alloc_context3" \
    --allowlist-function "avcodec_receive_frame" \
    --allowlist-function "avcodec_send_packet" \
    --allowlist-function "avformat_open_input" \
    --allowlist-function "avformat_close_input" \
    --allowlist-function "avformat_find_stream_info" \
    --allowlist-function "av_find_input_format" \
    --allowlist-function "avio_alloc_context" \
    --allowlist-function "avio_context_free" \
    --allowlist-function "av_frame_alloc" \
    --allowlist-function "av_frame_free" \
    --allowlist-function "av_frame_get_side_data" \
    --allowlist-function "av_frame_unref" \
    --allowlist-function "av_packet_alloc" \
    --allowlist-function "av_packet_free" \
    --allowlist-function "av_packet_unref" \
    --allowlist-function "av_free" \
    --allowlist-function "av_malloc" \
    --allowlist-function "av_log_set_level" \
    --allowlist-function "av_get_bits_per_pixel" \
    --allowlist-function "av_pix_fmt_desc_get" \
    --allowlist-function "av_get_sample_fmt_name" \
    --allowlist-function "av_get_sample_fmt" \
    --allowlist-function "av_get_alt_sample_fmt" \
    --allowlist-function "av_get_channel_layout_nb_channels" \
    --allowlist-function "av_get_default_channel_layout" \
    --allowlist-function "sws_getContext" \
    --allowlist-function "sws_freeContext" \
    --allowlist-function "sws_scale" \
    --allowlist-function "swr_alloc" \
    --allowlist-function "swr_init" \
    --allowlist-function "swr_free" \
    --allowlist-function "swr_convert" \
    \
    --allowlist-var "AV_LOG_INFO" \
    --allowlist-var "AV_LOG_QUIET" \
    --allowlist-var "AVERROR_EOF" \
    --allowlist-var "AVFMT_FLAG_CUSTOM_IO" \
    --allowlist-var "AVFMT_FLAG_NOFILLIN" \
    --allowlist-var "AVMEDIA_TYPE_VIDEO" \
    --allowlist-var "AVSEEK_FLAG_BACKWARD" \
    --allowlist-var "AVSEEK_SIZE" \
    --allowlist-var "AV_PIX_FMT_RGB24" \
    --allowlist-var "AV_PIX_FMT_RGBA" \
    --allowlist-var "AV_PIX_FMT_FLAG_ALPHA" \
    --allowlist-var "AV_FRAME_DATA_DISPLAYMATRIX" \
    --allowlist-var "AV_STREAM_GROUP_PARAMS_TILE_GRID" \
    \
    --allowlist-type "AVCodec" \
    --allowlist-type "AVCodecContext" \
    --allowlist-type "AVCodecID" \
    --allowlist-type "AVFormatContext" \
    --allowlist-type "AVInputFormat" \
    --allowlist-type "AVIOContext" \
    --allowlist-type "AVPacket" \
    --allowlist-type "AVFrame" \
    --allowlist-type "AVFrameSideDataType" \
    --allowlist-type "AVMediaType" \
    --allowlist-type "AVPixelFormat" \
    --allowlist-type "AVSampleFormat" \
    --allowlist-type "AVStream" \
    --allowlist-type "AVStreamGroupParamsType" \
    --allowlist-type "SwsContext" \
    --allowlist-type "SwrContext" \
    \
    --no-layout-tests \
    --no-doc-comments \
    --use-core \
    --ctypes-prefix "libc" \
    --output "${OUTPUT}"

# Fix Rust 2024: extern blocks must be unsafe.
sed -i 's/^extern "C"/unsafe extern "C"/' "${OUTPUT}"

echo "[bindings] Done: $(wc -l < "${OUTPUT}") lines"
echo "[bindings] Copy to repo if this is a new platform target."
