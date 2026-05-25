#!/usr/bin/env bash
# Build a minimal, statically-linkable FFmpeg for the thumbrella tier2 binary.
#
# Goals
#  - Software decoders and demuxers (no hardware acceleration)
#  - Static archives (.a) installed to /opt/ffmpeg-static
#  - No network protocols (we handle HTTP via reqwest)
#  - No encoders, muxers, filters, or device APIs
#
# External libraries (also built statically):
#  - dav1d  — fast AV1 decoder (used for AVIF still images)
#
# After this script runs, set FFMPEG_DIR=/opt/ffmpeg-static and build tier2
# with the `static` Cargo feature on ffmpeg-sys-next.

set -euo pipefail

PREFIX=/opt/ffmpeg-static
BUILD_DEPS_DIR=/tmp/thumbrella-deps

# ── dav1d (AV1 decoder) ─────────────────────────────────────────────────────
DAV1D_VERSION=1.5.1

echo "[ffmpeg-static] Building dav1d ${DAV1D_VERSION}..."
mkdir -p "${BUILD_DEPS_DIR}"

if [[ ! -f "${BUILD_DEPS_DIR}/dav1d-${DAV1D_VERSION}/build/src/libdav1d.a" ]]; then
    rm -rf "${BUILD_DEPS_DIR}/dav1d-${DAV1D_VERSION}"
    curl -L --retry 3 -o "${BUILD_DEPS_DIR}/dav1d.tar.gz" \
        "https://code.videolan.org/videolan/dav1d/-/archive/${DAV1D_VERSION}/dav1d-${DAV1D_VERSION}.tar.gz"
    tar -xzf "${BUILD_DEPS_DIR}/dav1d.tar.gz" -C "${BUILD_DEPS_DIR}"
    cd "${BUILD_DEPS_DIR}/dav1d-${DAV1D_VERSION}"
    meson setup build \
        --prefix="${PREFIX}" \
        --libdir=lib \
        --default-library=static \
        -Denable_tools=false \
        -Denable_tests=false \
        -Denable_examples=false \
        --buildtype=release
    ninja -C build
    ninja -C build install
    echo "[ffmpeg-static] dav1d installed."
else
    echo "[ffmpeg-static] dav1d already built — skipping."
fi

# Ensure FFmpeg's configure can find dav1d via pkg-config.
export PKG_CONFIG_PATH="${PREFIX}/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

# ── FFmpeg ──────────────────────────────────────────────────────────────────
FFMPEG_VERSION=7.1
FFMPEG_TARBALL=ffmpeg-${FFMPEG_VERSION}.tar.gz
FFMPEG_SRC_URL="https://ffmpeg.org/releases/${FFMPEG_TARBALL}"
FFMPEG_BUILD_DIR=/tmp/ffmpeg-build

# ── Download ──────────────────────────────────────────────────────────────────
echo "[ffmpeg-static] Downloading FFmpeg ${FFMPEG_VERSION}..."
mkdir -p "${FFMPEG_BUILD_DIR}"
if [[ ! -f "${FFMPEG_BUILD_DIR}/${FFMPEG_TARBALL}" ]]; then
    curl -L --retry 3 -o "${FFMPEG_BUILD_DIR}/${FFMPEG_TARBALL}" "${FFMPEG_SRC_URL}"
fi

echo "[ffmpeg-static] Extracting..."
tar -xf "${FFMPEG_BUILD_DIR}/${FFMPEG_TARBALL}" -C "${FFMPEG_BUILD_DIR}" --strip-components=1 --overwrite

cd "${FFMPEG_BUILD_DIR}"

# ── Configure ─────────────────────────────────────────────────────────────────
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
    --disable-avdevice \
    --disable-postproc \
    --disable-avfilter \
    --disable-network \
    \
    --disable-everything \
    \
    --enable-zlib \
    --enable-bzlib \
    --enable-lzma \
    --enable-libdav1d \
    \
    --disable-autodetect \
    \
    --enable-decoder=h264,hevc,vp8,vp9,libdav1d,mpeg1video,mpeg2video,mpeg4,\
msmpeg4v1,msmpeg4v2,msmpeg4v3,h263,h263p,flv1,wmv1,wmv2,wmv3,vc1,\
mjpeg,jpeg2000,png,gif,bmp,tiff,webp,theora,dirac,dnxhd,dnxhr,prores,\
hap,svq1,svq3,rv10,rv20,rv30,rv40,indeo2,indeo3,indeo4,indeo5,\
huffyuv,ffv1,utvideo,zlib,qtrle,rpza,smc,8bps,aura,aura2,\
dds,psd,\
rawvideo,pam,pbm,pgm,pgmyuv,ppm,sunrast,targa,xbm,\
aac,ac3,eac3,mp2,mp3,opus,vorbis,flac,pcm_s16le,pcm_s16be,pcm_s24le,\
pcm_s32le,pcm_u8,pcm_alaw,pcm_mulaw,pcm_f32le \
    \
    --disable-decoder=av1 \
    \
    --enable-demuxer=mov,mp4,m4v,matroska,webm,avi,mpegts,mpegps,mpegvideo,\
flv,asf,rm,rmvb,ogg,mxf,gxf,lxf,yuv4mpegpipe,rawvideo,\
image2,gif,image_jpeg_pipe,image_png_pipe,image_bmp_pipe,image_tiff_pipe,ico,\
image_webp_pipe,image_psd_pipe,dds,\
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
    --extra-cflags="-O3 -fPIC" \
    --extra-cxxflags="-O3 -fPIC" \
    2>&1

# ── Build ─────────────────────────────────────────────────────────────────────
JOBS=$(nproc)
echo "[ffmpeg-static] Building with ${JOBS} jobs..."
make -j"${JOBS}" 2>&1

echo "[ffmpeg-static] Installing to ${PREFIX}..."
make install 2>&1

# ── Verify ────────────────────────────────────────────────────────────────────
echo "[ffmpeg-static] Installed files:"
ls -lh "${PREFIX}/lib/"*.a

echo ""
echo "[ffmpeg-static] Done.  Set FFMPEG_DIR=${PREFIX} before building tier2."
