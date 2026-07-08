//! Pipeline step: **inspect** — sniff file type and determine processing tier.

use std::io::Cursor;

use image::{ImageDecoder, ImageReader};

use crate::cook::ThumbCook;
use crate::dispatch;
use crate::http_buf::HttpStream;
use crate::media::FileKind;

/// Bytes read from the front of the body for type sniffing.
/// 4 KiB covers all `infer` magic signatures comfortably.
const SNIFF_LEN: usize = 4 * 1024;

/// Pull the first bytes of the body, sniff the file type, and determine
/// the processing tier.
///
/// Must only be called after `connect` has opened the HTTP connection.
///
/// Populates:
/// - `cook.media.mime`         — sniffed MIME type string
/// - `cook.media.kind`         — coarse `FileKind` category
/// - `cook.media.extension`    — canonical extension (no dot)
/// - `cook.media.properties`   — `{width_pixels, height_pixels, bits_per_pixel}` for `Image` kind
pub async fn inspect<S: HttpStream>(cook: &mut ThumbCook<S>) {
    if !cook.http_is_open() {
        return;
    }

    // read_at preserves the cursor — subsequent steps (shortcut, render)
    // continue from byte 0.
    let prefix = match cook.http_read_at(0, SNIFF_LEN).await {
        Ok(b) => b,
        Err(e) => {
            cook.fail(format!("inspect read error: {e}"));
            return;
        }
    };

    let content_type = cook.http_headers.get("content-type").cloned();
    let (kind, mime, extension) = sniff(&prefix, &cook.input.url, content_type.as_deref());
    cook.media.mime = Some(mime);
    cook.media.kind = Some(kind);
    cook.media.extension = Some(extension);
    cook.media.properties = Some(serde_json::json!({}));

    // For image formats, dimension headers are always within the first few KB.
    if kind == FileKind::Image {
        inspect_image_properties(&prefix, cook.media.properties.as_mut().unwrap());
    }

    // Audio lossless: inferred from extension.
    if kind == FileKind::Audio
        && let Some(props) = cook.media.properties.as_mut() {
            let obj = props.as_object_mut().expect("properties is always a JSON object");
            let ext = cook.media.extension.as_deref().unwrap_or("");
            obj.insert("lossless".into(), (is_lossless_audio_ext(ext) as i32).into());
        }

    // Route determines which tier should process this — informational only at
    // this point; the cook will escalate if needed during shortcut/render.
    let _route = dispatch::route(kind, cook.media.extension.as_deref());
}

//  Sniffing 

/// Identify the (kind, mime, extension) triple for a byte prefix.
///
/// Priority order:
/// 1. Magic bytes via `infer` — most reliable.
/// 2. HTTP `Content-Type` header — trusted server classification, used when
///    magic bytes produce nothing or only a generic container result.
/// 3. URL path extension — last resort when neither infer nor server helped.
fn sniff(bytes: &[u8], url: &str, content_type: Option<&str>) -> (FileKind, String, String) {
    let url_ext = url_extension(url);

    if let Some(t) = infer::get(bytes) {
        let infer_ext = canonical_extension(t.extension());
        let mut infer_kind = infer_matcher_to_kind(t.matcher_type());

        // infer classifies HTML as MatcherType::Text, but HTML is a document.
        if matches!(t.mime_type(), "text/html" | "application/xhtml+xml") {
            infer_kind = FileKind::Document;
        }

        // infer classifies SVG as text/xml (MatcherType::Text); sniff the
        // byte prefix for an <svg root element, falling back to URL extension.
        if infer_kind == FileKind::Text
            && (sniff_svg(bytes) || url_ext.as_deref() == Some("svg")) {
                return (FileKind::Vector, "image/svg+xml".to_string(), "svg".to_string());
            }

        // When magic bytes identify a generic container, prefer a more
        // specific kind from the URL extension (e.g. USDZ, DOCX are ZIP).
        if matches!(infer_kind, FileKind::Archive | FileKind::Binary | FileKind::Unknown)
            && let Some(ext) = &url_ext {
                let url_kind = ext_to_kind(ext);
                if !matches!(url_kind, FileKind::Archive | FileKind::Binary | FileKind::Unknown) {
                    let mime = ext_to_mime(ext).to_string();
                    return (url_kind, mime, ext.clone());
                }
            }

        // When magic bytes identify plain TIFF but the URL names a known
        // camera-raw format, prefer the raw extension.  This lets the
        // shortcut step choose the wider RAW_HEADER_SCAN and report the
        // correct format in traces ("shortcut/raw" vs "shortcut/exif").
        if infer_ext == "tiff"
            && let Some(ext) = &url_ext
                && is_raw_tiff_extension(ext) {
                    return (FileKind::Image, ext_to_mime(ext).to_string(), ext.clone());
                }

        return (infer_kind, t.mime_type().to_string(), infer_ext);
    }

    // infer found nothing — try the HTTP Content-Type header.
    if let Some(ct) = content_type {
        // Strip parameters like "; charset=utf-8".
        let mime = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
        let ct_ext = url_ext.clone().unwrap_or_else(|| mime_to_extension(&mime).to_string());
        let ct_kind = mime_to_kind(&mime, &ct_ext);
        if !matches!(ct_kind, FileKind::Unknown) {
            return (ct_kind, mime, ct_ext);
        }
    }

    if let Some(ext) = url_ext {
        let kind = ext_to_kind(&ext);
        let mime = ext_to_mime(&ext).to_string();
        return (kind, mime, ext);
    }

    (FileKind::Unknown, "application/octet-stream".to_string(), "bin".to_string())
}

//  Image property inspection

/// Extract pixel dimensions from an image byte prefix without a full decode.
///
/// For JPEG: manually skips APP marker segments in case the EXIF APP1 segment
/// is large enough to push the SOF marker beyond the available prefix (e.g.
/// files with embedded full-resolution thumbnails).  Falls back to the `image`
/// crate for all other formats.
///
/// Writes any properties it can determine into `props`, leaving existing keys
/// untouched if it cannot determine a value.  Prefer no entry over a wrong one.
pub(super) fn inspect_image_properties(bytes: &[u8], props: &mut serde_json::Value) {
    // JPEG: parse marker structure ourselves so we skip large APP segments.
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8
        && let Some((w, h, bpp)) = jpeg_sof_dimensions(bytes) {
            let obj = props.as_object_mut().expect("properties is always a JSON object");
            obj.insert("width".into(), w.into());
            obj.insert("height".into(), h.into());
            obj.insert("bpp".into(), bpp.into());
            obj.insert("alpha".into(), (0_i32).into());
            obj.insert("lossless".into(), (0_i32).into());
            return;
        }
        // SOF unreachable — fall through to image crate as a fallback.

    let cursor = Cursor::new(bytes);
    let Ok(reader) = ImageReader::new(cursor).with_guessed_format() else {
        return;
    };
    let format = reader.format();
    let Ok(decoder) = reader.into_decoder() else {
        return;
    };
    let (w, h) = decoder.dimensions();
    let ct = decoder.color_type();
    let per_channel = ct.bits_per_pixel() as u32 / ct.channel_count() as u32;
    // Count only colour channels, skipping alpha.  For RGB8 this is 24,
    // for RGBA8 it is still 24, for Gray8 it is 8.
    let color_channels = if ct.has_alpha() {
        ct.channel_count() as u32 - 1
    } else {
        ct.channel_count() as u32
    };
    let bits_per_pixel = per_channel * color_channels;
    let obj = props.as_object_mut().expect("properties is always a JSON object");
    obj.insert("width".into(), w.into());
    obj.insert("height".into(), h.into());
    obj.insert("bpp".into(), bits_per_pixel.into());
    obj.insert("alpha".into(), (ct.has_alpha() as i32).into());
    if let Some(fmt) = format {
        obj.insert("lossless".into(), (is_lossless(fmt) as i32).into());
    }
}

/// Whether the given image format uses lossless compression.
fn is_lossless(format: image::ImageFormat) -> bool {
    use image::ImageFormat;
    matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Bmp
            | ImageFormat::Tiff
            | ImageFormat::Gif
            | ImageFormat::Ico
            | ImageFormat::Tga
            | ImageFormat::OpenExr
    )
}

/// Whether the given audio extension implies lossless encoding.
fn is_lossless_audio_ext(ext: &str) -> bool {
    matches!(ext, "flac" | "wav" | "aiff" | "aif" | "alac" | "ape" | "wv")
}

/// Walk JPEG markers starting after SOI (offset 2), skipping APP segments
/// by their length field, and return `(width, height, bits_per_pixel)` from
/// the first SOF0/SOF1/SOF2 marker found.  Returns `None` if no SOF is
/// reachable within `bytes`.
pub(super) fn jpeg_sof_dimensions(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let mut pos: usize = 2;
    while pos + 8 < bytes.len() {
        if bytes[pos] != 0xFF {
            return None;
        }
        let marker = bytes[pos + 1];
        // Stuffed byte (FF 00)
        if marker == 0x00 {
            pos += 2;
            continue;
        }
        // SOF markers: baseline, extended sequential, progressive
        if matches!(marker, 0xC0..=0xC2) {
            let h = u16::from_be_bytes([bytes[pos + 5], bytes[pos + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[pos + 7], bytes[pos + 8]]) as u32;
            let components = bytes[pos + 9];
            let bpp = (components as u32) * (bytes[pos + 4] as u32);
            return Some((w, h, bpp));
        }
        // SOS — scan data follows, SOF must precede it
        if marker == 0xDA {
            return None;
        }
        // All other markers have a 2-byte length (includes the length field itself)
        if pos + 4 > bytes.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
        if seg_len < 2 {
            return None;
        }
        pos += 2 + seg_len;
    }
    None
}

/// Walk JPEG markers from a file prefix (starting at SOI) and return the byte
/// offset where APP segments end — the position just before DQT/DHT/SOF/SOS.
/// This is the offset from which a targeted read can reliably find the SOF
/// marker.  Returns `None` if the header is invalid or SOS is reached first.
pub(super) fn jpeg_app_segments_end(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos: usize = 2;
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF {
            return None;
        }
        let marker = bytes[pos + 1];
        if marker == 0x00 {
            pos += 2;
            continue;
        }
        // Terminal markers — stop walking, return current position
        if matches!(marker, 0xC0..=0xC2 | 0xC4 | 0xDB | 0xDA) {
            return Some(pos as u64);
        }
        let seg_len = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
        if seg_len < 2 {
            return None;
        }
        pos += 2 + seg_len;
    }
    // Fell off the end of the buffer — the SOF is beyond this prefix.
    None
}

/// Scan arbitrary bytes for a JPEG SOF marker and return dimensions.
/// Unlike [`jpeg_sof_dimensions`], this does not require the bytes to start
/// at SOI — it searches for the first FF C0/C1/C2 marker.
pub(super) fn find_sof_in_bytes(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    for i in 0..bytes.len().saturating_sub(9) {
        if bytes[i] == 0xFF && matches!(bytes[i + 1], 0xC0..=0xC2) {
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            let bpp = (bytes[i + 9] as u32) * (bytes[i + 4] as u32);
            if w > 0 && h > 0 {
                return Some((w, h, bpp));
            }
        }
    }
    None
}

//  Extension helpers

/// Return `true` if `bytes` contain an `<svg` opening tag (case-insensitive),
/// indicating the content is an SVG document regardless of the MIME label.
fn sniff_svg(bytes: &[u8]) -> bool {
    // Match `<svg` followed by whitespace, '>', or '/' to avoid false positives
    // on longer element names like `<svgfoo`.
    bytes.windows(5).any(|w| {
        w[0] == b'<'
            && w[1].eq_ignore_ascii_case(&b's')
            && w[2].eq_ignore_ascii_case(&b'v')
            && w[3].eq_ignore_ascii_case(&b'g')
            && matches!(w[4], b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
    })
}

/// Return `true` for TIFF-container camera-raw extensions.
/// Used to prefer URL-derived extensions over the generic `"tiff"` label
/// that the `infer` crate assigns to all TIFF-magic files.
fn is_raw_tiff_extension(ext: &str) -> bool {
    matches!(
        ext,
        "dng"
            | "cr2"
            | "nef"
            | "arw"
            | "orf"
            | "rw2"
            | "pef"
            | "srw"
            | "raf"
            | "3fr"
            | "fff"
            | "iiq"
            | "mef"
            | "rwl"
            | "raw"
    )
}

/// Normalise common extension aliases to their canonical form.
/// Unknown extensions are returned as-is.
pub fn canonical_extension(raw: &str) -> String {
    match raw {
        // image formats
        "jpg" | "jpe" => "jpeg",
        "tif" => "tiff",
        "j2k" => "jp2",
        "sxr" | "mxr" => "exr",
        "rgbe" => "hdr",
        "rgb" | "rgba" | "int" | "inta" => "sgi",
        "pgm" | "ppm" | "pfm" => "pbm",
        "heics" => "heic",
        "heif" => "heic",
        // video / audio
        "mpg" | "mpe" => "mpeg",
        "m4v" => "mp4",
        "mid" => "midi",
        "aifc" => "aiff",
        // documents
        "doc" => "docx",
        "xls" => "xlsx",
        "xlsm" => "xlsx",
        "ppt" => "pptx",
        // vector
        "svg" | "svgz" => "svg",
        // 3D geometry
        "wrl" => "vrml",
        "stp" | "stpnc" | "p21" | "210" => "step",
        "igs" => "iges",
        "ex2" | "e" => "exo",
        "usda" | "usdc" => "usd",
        // camera raw — uncommon aliases
        "crw" => "cr2",
        // web
        "htm" => "html",
        other => other,
    }
    .to_string()
}

/// Extract and normalise the file extension from a URL path.
/// Returns `None` only when there is no dot-separated segment at all.
fn url_extension(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    let last = path.rsplit('/').next().unwrap_or("");
    let raw = last.rsplit('.').next().filter(|e| !e.is_empty() && *e != last)?;
    Some(canonical_extension(&raw.to_ascii_lowercase()))
}

//  Kind / MIME tables 

/// Map an `infer` `MatcherType` directly to a `FileKind`.
///
/// Preferred over `mime_to_kind` on the infer branch — uses the same
/// classification the library already committed to rather than re-parsing
/// the mime string.
fn infer_matcher_to_kind(mt: infer::MatcherType) -> FileKind {
    match mt {
        infer::MatcherType::Image => FileKind::Image,
        infer::MatcherType::Video => FileKind::Video,
        infer::MatcherType::Audio => FileKind::Audio,
        infer::MatcherType::Archive => FileKind::Archive,
        infer::MatcherType::Doc => FileKind::Document,
        infer::MatcherType::Text => FileKind::Text,
        infer::MatcherType::Book => FileKind::Document,
        infer::MatcherType::App | infer::MatcherType::Font | infer::MatcherType::Custom => FileKind::Binary,
    }
}

fn mime_to_kind(mime: &str, ext: &str) -> FileKind {
    if mime.starts_with("image/") {
        return if ext == "svg" { FileKind::Vector } else { FileKind::Image };
    }
    if mime.starts_with("video/") {
        return FileKind::Video;
    }
    if mime.starts_with("audio/") {
        return FileKind::Audio;
    }
    // HTML is a document, not plain text, despite the text/ prefix.
    if matches!(mime, "text/html" | "application/xhtml+xml") {
        return FileKind::Document;
    }
    if mime.starts_with("text/") {
        return FileKind::Text;
    }
    match mime {
        "application/pdf"
        | "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        | "application/vnd.oasis.opendocument.text"
        | "application/vnd.oasis.opendocument.presentation"
        | "application/vnd.oasis.opendocument.spreadsheet"
        | "application/vnd.ms-excel"
        | "application/vnd.ms-powerpoint"
        | "application/epub+zip"
        | "application/rtf" => FileKind::Document,
        "application/zip"
        | "application/x-tar"
        | "application/gzip"
        | "application/x-bzip2"
        | "application/x-xz"
        | "application/vnd.rar"
        | "application/x-7z-compressed" => FileKind::Archive,
        "model/vnd.usdz+zip"
        | "model/gltf-binary"
        | "model/gltf+json"
        | "application/vnd.fbx"
        | "model/vnd.collada+xml"
        | "model/obj"
        | "model/stl" => FileKind::Geometry,
        _ => ext_to_kind(ext),
    }
}

fn ext_to_kind(ext: &str) -> FileKind {
    match ext {
        "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff" | "avif" | "heic" | "exr" | "hdr" | "apng"
        | "ico" | "xcf" | "jxl" | "psd" | "pbm" | "tga" => FileKind::Image,
        // JPEG 2000
        "jp2" => FileKind::Image,
        // Studio / VFX image formats (via oiiotool).
        "sgi" | "dpx" | "cin" | "dds" | "fits" | "iff" | "pic" | "rla" | "zfile" => FileKind::Image,
        // Camera-raw formats (TIFF-based containers).
        "dng" | "cr2" | "nef" | "arw" | "orf" | "rw2" | "pef" | "srw" | "3fr" | "mef" | "rwl" | "raf"
        | "fff" | "iiq" | "raw" => FileKind::Image,
        "svg" | "emf" | "wmf" => FileKind::Vector,
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "mpeg" | "ogv" | "flv" | "ts" | "3gp" | "wmv" | "m2ts"
        | "mxf" | "av1" => FileKind::Video,
        "mp3" | "ogg" | "flac" | "wav" | "m4a" | "aac" | "midi" | "opus" | "oga" | "weba" | "wma"
        | "aiff" => FileKind::Audio,
        "pdf" | "docx" | "xlsx" | "pptx" | "odt" | "doc" | "xls" | "ppt" | "odp" | "ods" | "epub" | "rtf"
        | "html" | "xhtml" => FileKind::Document,
        "usdz" | "usd" | "glb" | "gltf" | "obj" | "stl" | "fbx" | "dae" | "dxf" | "off" | "3ds" | "gml"
        | "ply" | "pts" | "vrml" | "vtk" | "vtu" | "vtp" | "vti" | "vtr" | "vts" | "vtm" | "step"
        | "iges" | "brep" | "exo" => FileKind::Geometry,
        "zip" | "tar" | "gz" | "bz2" | "xz" | "rar" | "7z" => FileKind::Archive,
        "xml" | "json" | "txt" | "csv" | "md" => FileKind::Text,
        _ => FileKind::Unknown,
    }
}

/// Best-guess extension for a MIME type, used when Content-Type is the only
/// signal and we have no URL extension.
fn mime_to_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/avif" => "avif",
        "image/heic" => "heic",
        "image/heif" => "heic",
        "image/svg+xml" => "svg",
        "image/emf" => "emf",
        "image/x-xcf" => "xcf",
        "image/apng" => "apng",
        "image/vnd.microsoft.icon" => "ico",
        "image/jxl" => "jxl",
        "image/vnd.adobe.photoshop" => "psd",
        "image/jp2" => "jp2",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/mpeg" => "mpeg",
        "video/ogg" => "ogv",
        "video/x-flv" => "flv",
        "video/mp2t" => "ts",
        "video/3gpp" => "3gp",
        "video/x-ms-wmv" => "wmv",
        "video/mxf" => "mxf",
        "video/av1" => "av1",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/wav" => "wav",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        "audio/midi" | "audio/x-midi" => "midi",
        "audio/webm" => "weba",
        "audio/x-ms-wma" => "wma",
        "audio/aiff" => "aiff",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.oasis.opendocument.text" => "odt",
        "application/vnd.oasis.opendocument.presentation" => "odp",
        "application/vnd.oasis.opendocument.spreadsheet" => "ods",
        "application/msword" => "doc",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/epub+zip" => "epub",
        "application/rtf" => "rtf",
        "model/vnd.usdz+zip" => "usdz",
        "model/gltf-binary" => "glb",
        "model/gltf+json" => "gltf",
        "model/obj" => "obj",
        "application/vnd.fbx" => "fbx",
        "model/vnd.collada+xml" => "dae",
        "model/stl" => "stl",
        "text/html" => "html",
        "text/xml" => "xml",
        "text/plain" => "txt",
        "text/csv" => "csv",
        "text/markdown" => "md",
        "application/json" => "json",
        _ => "bin",
    }
}

fn ext_to_mime(ext: &str) -> &'static str {
    match ext {
        "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" => "image/tiff",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "exr" => "image/x-exr",
        "hdr" => "image/vnd.radiance",
        "jxl" => "image/jxl",
        "psd" => "image/vnd.adobe.photoshop",
        "jp2" => "image/jp2",
        "pbm" => "image/x-portable-bitmap",
        "tga" => "image/x-tga",
        "xcf" => "image/x-xcf",
        "sgi" => "image/x-sgi",
        "dpx" => "image/x-dpx",
        "cin" => "image/x-cineon",
        "dds" => "image/vnd-ms.dds",
        "fits" => "image/fits",
        "iff" => "image/x-iff",
        "dng" => "image/x-adobe-dng",
        "cr2" => "image/x-canon-cr2",
        "nef" => "image/x-nikon-nef",
        "arw" => "image/x-sony-arw",
        "orf" => "image/x-olympus-orf",
        "rw2" => "image/x-panasonic-rw2",
        "pef" => "image/x-pentax-pef",
        "srw" => "image/x-samsung-srw",
        "3fr" => "image/x-hasselblad-3fr",
        "mef" => "image/x-mamiya-mef",
        "rwl" => "image/x-leica-rwl",
        "raf" => "image/x-fuji-raf",
        "fff" => "image/x-hasselblad-fff",
        "iiq" => "image/x-phaseone-iiq",
        "raw" => "image/x-raw",
        "svg" => "image/svg+xml",
        "emf" => "image/emf",
        "wmf" => "image/wmf",
        "apng" => "image/apng",
        "ico" => "image/vnd.microsoft.icon",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "webm" => "video/webm",
        "mpeg" => "video/mpeg",
        "ogv" => "video/ogg",
        "flv" => "video/x-flv",
        "ts" => "video/mp2t",
        "3gp" => "video/3gpp",
        "wmv" => "video/x-ms-wmv",
        "m2ts" => "video/mp2t",
        "mxf" => "video/mxf",
        "av1" => "video/av1",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "midi" => "audio/midi",
        "opus" => "audio/ogg",
        "oga" => "audio/ogg",
        "weba" => "audio/webm",
        "wma" => "audio/x-ms-wma",
        "aiff" => "audio/aiff",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "doc" => "application/msword",
        "xls" => "application/vnd.ms-excel",
        "ppt" => "application/vnd.ms-powerpoint",
        "epub" => "application/epub+zip",
        "rtf" => "application/rtf",
        "html" => "text/html",
        "xhtml" => "application/xhtml+xml",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "xml" => "text/xml",
        "json" => "application/json",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "usdz" => "model/vnd.usdz+zip",
        "usd" => "model/vnd.usdz+zip",
        "glb" => "model/gltf-binary",
        "gltf" => "model/gltf+json",
        "obj" => "model/obj",
        "fbx" => "application/vnd.fbx",
        "dae" => "model/vnd.collada+xml",
        "stl" => "model/stl",
        "vrml" => "model/vrml",
        "step" => "model/step",
        "iges" => "model/iges",
        _ => "application/octet-stream",
    }
}
