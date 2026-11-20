#!/usr/bin/env bash
# Build a minimal, statically-linkable FFmpeg for the thumbrella tier2 binary.
#
# Goals
#  - Software decoders and demuxers (no hardware acceleration)
#  - Static archives (.a) installed to ffs/build (or --prefix DIR)
#  - No network protocols (we handle HTTP via reqwest)
#  - No encoders, muxers, filters, or device APIs
#  - libdav1d (BSD-2-Clause) is built statically for software AV1/AVIF decode.
#    FFmpeg detects it via pkg-config and links it as a separate archive;
#    the extra link args go into the generated .cargo/ffs.toml.
#
# Usage:
#   ./ffs/build-linux.sh                       # -> ffs/build
#   ./ffs/build-linux.sh --prefix ~/ffmpeg      # custom dir
#
# After this script runs, cargo build just works, PKG_CONFIG_PATH
# is configured in the generated .cargo/ffs.toml.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PREFIX="${PROJECT_ROOT}/ffs/build"
if [[ "${1:-}" == "--prefix" ]]; then
    PREFIX="${2:?missing --prefix argument}"
fi

#  Required tools
#
# meson/ninja build dav1d; nasm assembles its SIMD paths (mandatory for
# usable AV1 decode speed).
for tool in meson ninja nasm; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "[ffmpeg-static] missing required tool: ${tool}" >&2
        exit 1
    fi
done

#  FFmpeg 
FFMPEG_VERSION=9.0.2
FFMPEG_TARBALL=ffmpeg-${FFMPEG_VERSION}.tar.gz
FFMPEG_SRC_URL="https://ffmpeg.org/releases/${FFMPEG_TARBALL}"
FFMPEG_BUILD_DIR="${PROJECT_ROOT}/target/ffmpeg-build/${FFMPEG_VERSION}"

#  dav1d 
#
# Software AV1 decoder used by FFmpeg for AVIF and AV1 video.  FFmpeg's own
# "av1" decoder is a hardware-acceleration front-end only, so the software
# path must come from libdav1d.
DAV1D_VERSION=1.5.5
DAV1D_TARBALL=dav1d-${DAV1D_VERSION}.tar.gz
DAV1D_SRC_URL="https://code.videolan.org/videolan/dav1d/-/archive/${DAV1D_VERSION}/${DAV1D_TARBALL}"
DAV1D_BUILD_DIR="${PROJECT_ROOT}/target/dav1d-build/${DAV1D_VERSION}"

#  Download 
echo "[ffmpeg-static] Downloading FFmpeg ${FFMPEG_VERSION}..."
mkdir -p "${FFMPEG_BUILD_DIR}"
if [[ ! -f "${FFMPEG_BUILD_DIR}/${FFMPEG_TARBALL}" ]]; then
    curl --fail -L --retry 3 -o "${FFMPEG_BUILD_DIR}/${FFMPEG_TARBALL}" "${FFMPEG_SRC_URL}"
fi

echo "[ffmpeg-static] Extracting..."
tar -xf "${FFMPEG_BUILD_DIR}/${FFMPEG_TARBALL}" -C "${FFMPEG_BUILD_DIR}" --strip-components=1 --overwrite

#  dav1d download/build
#
# Built and installed into the same prefix as FFmpeg so configure's pkg-config
# probe finds it, and so libdav1d.a sits next to the libav archives for the
# link step.  dav1d is a decoder only; tools, tests, examples and docs are
# disabled.
if [[ ! -f "${DAV1D_BUILD_DIR}/build/src/libdav1d.a" || ! -f "${PREFIX}/lib/libdav1d.a" ]]; then
    echo "[ffmpeg-static] Downloading dav1d ${DAV1D_VERSION}..."
    rm -rf "${DAV1D_BUILD_DIR}"
    mkdir -p "${DAV1D_BUILD_DIR}"
    curl --fail -L --retry 3 -o "${DAV1D_BUILD_DIR}/${DAV1D_TARBALL}" "${DAV1D_SRC_URL}"

    echo "[ffmpeg-static] Extracting dav1d..."
    tar -xzf "${DAV1D_BUILD_DIR}/${DAV1D_TARBALL}" -C "${DAV1D_BUILD_DIR}" --strip-components=1

    echo "[ffmpeg-static] Building dav1d (static, decoder only)..."
    (
        cd "${DAV1D_BUILD_DIR}"
        meson setup build \
            --prefix="${PREFIX}" \
            --libdir=lib \
            --default-library=static \
            --buildtype=release \
            -Denable_tools=false \
            -Denable_tests=false \
            -Denable_examples=false \
            2>&1
        ninja -C build 2>&1
        ninja -C build install 2>&1
    )
else
    echo "[ffmpeg-static] dav1d already built - reinstalling."
    ninja -C "${DAV1D_BUILD_DIR}/build" install 2>&1
fi

cd "${FFMPEG_BUILD_DIR}"

#  Configure
#
# dav1d lives in the same prefix as FFmpeg, so point pkg-config there for
# --enable-libdav1d.  The native "av1" decoder is deliberately not enabled:
# it only drives hardware acceleration and cannot decode in software.
export PKG_CONFIG_PATH="${PREFIX}/lib/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"

echo "[ffmpeg-static] Configuring..."
./configure \
    --prefix="${PREFIX}" \
    \
    --disable-shared \
    --enable-static \
    --enable-pic \
    \
    --disable-programs \
    --disable-doc \
    \
    --enable-avdevice \
    --enable-avfilter \
    --disable-network \
    \
    --disable-everything \
    --disable-autodetect \
    \
    --enable-zlib \
    --enable-libdav1d \
    --enable-decoder=h264,hevc,vp8,vp9,libdav1d,mpeg1video,mpeg2video,mpeg4,\
msmpeg4v1,msmpeg4v2,msmpeg4v3,h263,h263p,flv,vp6,cinepak,wmv1,wmv2,wmv3,vc1,\
mjpeg,jpeg2000,png,gif,bmp,tiff,webp,theora,dirac,dnxhd,prores,dvvideo,\
hap,svq1,svq3,rv10,rv20,rv30,rv40,indeo2,indeo3,indeo4,indeo5,\
huffyuv,ffv1,utvideo,zlib,qtrle,rpza,smc,eightbps,aura,aura2,\
dds,psd,qoi,xwd,hdr,\
rawvideo,pam,pbm,pgm,pgmyuv,ppm,sunrast,targa,xbm,\
aac,ac3,eac3,mp2,mp3,opus,vorbis,flac,pcm_s16le,pcm_s16be,pcm_s24le,\
pcm_s32le,pcm_u8,pcm_alaw,pcm_mulaw,pcm_f32le \
    \
    --disable-decoder=av1 \
    \
    --enable-demuxer=mov,m4v,matroska,avi,mpegts,mpegps,mpegvideo,\
flv,asf,rm,ogg,mxf,gxf,lxf,yuv4mpegpipe,rawvideo,\
image2,image2pipe,gif,image_jpeg_pipe,image_png_pipe,image_bmp_pipe,\
image_tiff_pipe,ico,image_webp_pipe,image_psd_pipe,image_qoi_pipe,\
image_xwd_pipe,image_hdr_pipe,image_pam_pipe,image_j2k_pipe,\
ape,aiff,au,wav,mp3,aac,flac,ogg \
    \
    --enable-parser=h264,hevc,vp8,vp9,av1,mpeg4video,mpeg4,mpegaudio,\
aac,flac,opus,vorbis,png,gif \
    \
    --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,mpeg4_unpack_bframes \
    \
    --enable-protocol=file,pipe,data \
    \
    --enable-swscale \
    --enable-swresample \
    \
    --optflags="-Os" \
    --extra-cflags="-fPIC" \
    --extra-cxxflags="-fPIC" \
    2>&1

#  Build
JOBS=$(nproc)
echo "[ffmpeg-static] Building with ${JOBS} jobs..."
make -j"${JOBS}" 2>&1

echo "[ffmpeg-static] Installing to ${PREFIX}..."
make install 2>&1

#  Verify 
echo "[ffmpeg-static] Installed files:"
ls -lh "${PREFIX}/lib/"*.a

#  Write .cargo/ffs.toml
#
# Absolute paths so cargo finds this install, plus the extra link args the
# configure options require.  ffmpeg-sys-next links the seven libav archives
# by name when FFMPEG_DIR is set and never reads the pkg-config files, so
# nothing else pulls in libdav1d.a even though libavcodec.a references
# dav1d_* symbols.  .cargo/config.toml includes this file and cargo merges
# rustflags from an included config file, so these are added to the existing
# target rustflags rather than replacing them.
HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
CARGO_DIR="${PROJECT_ROOT}/.cargo"
mkdir -p "${CARGO_DIR}"
cat > "${CARGO_DIR}/ffs.toml" << FFSTOML
# Generated by ffs/build-linux.sh, do not edit
[env]
PKG_CONFIG_PATH = { value = "${PREFIX}/lib/pkgconfig", force = true }
FFMPEG_DIR = { value = "${PREFIX}", force = true }

[target.${HOST_TRIPLE}]
rustflags = [
    "-C", "link-arg=-L${PREFIX}/lib",
    "-C", "link-arg=-ldav1d",
]
FFSTOML
echo "[ffmpeg-static] Wrote .cargo/ffs.toml (link args: -L${PREFIX}/lib -ldav1d)"
echo "[ffmpeg-static] Done.  Run: cargo build -p tier2"
