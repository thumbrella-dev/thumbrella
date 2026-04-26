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
/// Must only be called after `connect` has populated `cook.http`.
///
/// Populates:
/// - `cook.response.mime`       — sniffed MIME type string
/// - `cook.response.kind`       — coarse `FileKind` category
/// - `cook.response.extension`  — canonical extension (no dot)
/// - `cook.response.properties` — `{width, height}` for `Image` kind (best-effort)
/// - `cook.trace.job_tier`      — from `dispatch::route()`
pub async fn inspect<S: HttpStream>(cook: &mut ThumbCook<S>) {
    let Some(http) = cook.http.as_mut() else { return };

    // read_at preserves the cursor — subsequent steps (shortcut, render)
    // continue from byte 0.
    let prefix = match http.read_at(0, SNIFF_LEN).await {
        Ok(b) => b,
        Err(e) => {
            cook.fail(format!("inspect read error: {e}"));
            cook.http = None;
            return;
        }
    };

    let content_type = cook.http.as_ref()
        .and_then(|h| h.headers.get("content-type").cloned());
    let (kind, mime, extension) = sniff(&prefix, &cook.spec.url, content_type.as_deref());
    cook.response.mime = Some(mime);
    cook.response.kind = Some(kind);
    cook.response.extension = Some(extension);
    cook.response.properties = Some(serde_json::json!({}));

    // For image formats, dimension headers are always within the first few KB.
    if kind == FileKind::Image {
        inspect_image_properties(&prefix, cook.response.properties.as_mut().unwrap());
    }

    let route = dispatch::route(kind, cook.response.extension.as_deref());
    cook.trace.job_tier = route.tier;
}

// ── Sniffing ──────────────────────────────────────────────────────────────────

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
        let infer_kind = infer_matcher_to_kind(t.matcher_type());

        // When magic bytes identify a generic container, prefer a more
        // specific kind from the URL extension (e.g. USDZ, DOCX are ZIP).
        if matches!(infer_kind, FileKind::Archive | FileKind::Binary | FileKind::Unknown) {
            if let Some(ext) = &url_ext {
                let url_kind = ext_to_kind(ext);
                if !matches!(url_kind, FileKind::Archive | FileKind::Binary | FileKind::Unknown) {
                    let mime = ext_to_mime(ext).to_string();
                    return (url_kind, mime, ext.clone());
                }
            }
        }

        return (infer_kind, t.mime_type().to_string(), infer_ext);
    }

    // infer found nothing — try the HTTP Content-Type header.
    if let Some(ct) = content_type {
        // Strip parameters like "; charset=utf-8".
        let mime = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
        let ct_ext = url_ext.clone()
            .unwrap_or_else(|| mime_to_extension(&mime).to_string());
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

// ── Image property inspection ─────────────────────────────────────────────────

/// Extract pixel dimensions from an image byte prefix without a full decode.
///
/// Dimension headers are always within the first few KB for all formats the
/// `image` crate supports, so this works on the same prefix read by `inspect`
/// with no additional network I/O.
///
/// Writes any properties it can determine into `props`, leaving existing keys
/// untouched if it cannot determine a value.  Prefer no entry over a wrong one.
pub(super) fn inspect_image_properties(bytes: &[u8], props: &mut serde_json::Value) {
    let cursor = Cursor::new(bytes);
    let Ok(reader) = ImageReader::new(cursor).with_guessed_format() else { return };
    let Ok(mut decoder) = reader.into_decoder() else { return };
    let (w, h) = decoder.dimensions();
    let obj = props.as_object_mut().expect("properties is always a JSON object");
    obj.insert("width".into(),  w.into());
    obj.insert("height".into(), h.into());
}

// ── Extension helpers ─────────────────────────────────────────────────────────

/// Normalise common extension aliases to their canonical form.
/// Unknown extensions are returned as-is.
fn canonical_extension(raw: &str) -> String {
    match raw {
        "jpg"          => "jpeg",
        "tif"          => "tiff",
        "htm"          => "html",
        "mpg"          => "mpeg",
        "mid"          => "midi",
        "svg" | "svgz" => "svg",
        other          => other,
    }.to_string()
}

/// Extract and normalise the file extension from a URL path.
/// Returns `None` only when there is no dot-separated segment at all.
fn url_extension(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    let last = path.rsplit('/').next().unwrap_or("");
    let raw  = last.rsplit('.').next().filter(|e| !e.is_empty() && *e != last)?;
    Some(canonical_extension(&raw.to_ascii_lowercase()))
}

// ── Kind / MIME tables ────────────────────────────────────────────────────────

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
        infer::MatcherType::App | infer::MatcherType::Font
        | infer::MatcherType::Custom => FileKind::Binary,
    }
}

fn mime_to_kind(mime: &str, ext: &str) -> FileKind {
    if mime.starts_with("image/") {
        return if ext == "svg" { FileKind::Vector } else { FileKind::Image };
    }
    if mime.starts_with("video/") { return FileKind::Video; }
    if mime.starts_with("audio/") { return FileKind::Audio; }
    if mime.starts_with("text/")  { return FileKind::Text;  }
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
        | "model/obj"
        | "model/stl" => FileKind::Geometry,
        _ => ext_to_kind(ext),
    }
}

fn ext_to_kind(ext: &str) -> FileKind {
    match ext {
        "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff"
        | "avif" | "heic" | "heif" | "exr" | "hdr" | "dng"
        | "apng" | "ico"                                  => FileKind::Image,
        "svg"                                               => FileKind::Vector,
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "mpeg"
        | "ogv"                                            => FileKind::Video,
        "mp3" | "ogg" | "flac" | "wav" | "m4a"
        | "aac" | "midi" | "opus" | "oga" | "weba"        => FileKind::Audio,
        "pdf" | "docx" | "xlsx" | "pptx" | "odt"
        | "doc" | "xls" | "ppt" | "odp" | "ods"
        | "epub" | "rtf"                                  => FileKind::Document,
        "usdz" | "glb" | "gltf" | "obj" | "stl"           => FileKind::Geometry,
        "zip" | "tar" | "gz" | "bz2" | "xz" | "rar" | "7z" => FileKind::Archive,
        "html" | "xml" | "json" | "txt" | "csv" | "md"    => FileKind::Text,
        _                                                  => FileKind::Unknown,
    }
}

/// Best-guess extension for a MIME type, used when Content-Type is the only
/// signal and we have no URL extension.
fn mime_to_extension(mime: &str) -> &'static str {
    match mime {
        "image/jpeg"       => "jpeg", "image/png"        => "png",
        "image/gif"        => "gif",  "image/webp"       => "webp",
        "image/bmp"        => "bmp",  "image/tiff"       => "tiff",
        "image/avif"       => "avif", "image/heic"       => "heic",
        "image/heif"       => "heif", "image/svg+xml"    => "svg",
        "image/apng"       => "apng", "image/vnd.microsoft.icon" => "ico",
        "video/mp4"        => "mp4",  "video/quicktime"  => "mov",
        "video/webm"       => "webm", "video/mpeg"       => "mpeg",
        "video/ogg"        => "ogv",
        "audio/mpeg"       => "mp3",  "audio/ogg"        => "ogg",
        "audio/flac"       => "flac", "audio/wav"        => "wav",
        "audio/mp4"        => "m4a",  "audio/aac"        => "aac",
        "audio/midi" | "audio/x-midi" => "midi",
        "audio/webm"       => "weba",
        "application/pdf"  => "pdf",
        "application/zip"  => "zip",  "application/gzip" => "gz",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"       => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.oasis.opendocument.text"         => "odt",
        "application/vnd.oasis.opendocument.presentation" => "odp",
        "application/vnd.oasis.opendocument.spreadsheet"  => "ods",
        "application/msword"            => "doc",
        "application/vnd.ms-excel"      => "xls",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/epub+zip"          => "epub",
        "application/rtf"               => "rtf",
        "model/vnd.usdz+zip" => "usdz", "model/gltf-binary" => "glb",
        "model/gltf+json"    => "gltf", "model/obj"         => "obj",
        "model/stl"          => "stl",
        "text/html"  => "html", "text/xml"      => "xml",
        "text/plain" => "txt",  "text/csv"      => "csv",
        "text/markdown" => "md",
        "application/json" => "json",
        _ => "bin",
    }
}

fn ext_to_mime(ext: &str) -> &'static str {
    match ext {
        "jpeg" => "image/jpeg",   "png"  => "image/png",
        "gif"  => "image/gif",    "webp" => "image/webp",
        "bmp"  => "image/bmp",    "tiff" => "image/tiff",
        "avif" => "image/avif",   "heic" => "image/heic",
        "heif" => "image/heif",   "exr"  => "image/x-exr",
        "hdr"  => "image/vnd.radiance", "dng" => "image/x-adobe-dng",
        "svg"  => "image/svg+xml",
        "apng" => "image/apng",   "ico"  => "image/vnd.microsoft.icon",
        "mp4"  => "video/mp4",    "mov"  => "video/quicktime",
        "mkv"  => "video/x-matroska", "avi" => "video/x-msvideo",
        "webm" => "video/webm",   "mpeg" => "video/mpeg",
        "ogv"  => "video/ogg",
        "mp3"  => "audio/mpeg",   "ogg"  => "audio/ogg",
        "flac" => "audio/flac",   "wav"  => "audio/wav",
        "m4a"  => "audio/mp4",    "aac"  => "audio/aac",
        "midi" => "audio/midi",   "opus" => "audio/ogg",
        "oga"  => "audio/ogg",    "weba" => "audio/webm",
        "pdf"  => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt"  => "application/vnd.oasis.opendocument.text",
        "odp"  => "application/vnd.oasis.opendocument.presentation",
        "ods"  => "application/vnd.oasis.opendocument.spreadsheet",
        "doc"  => "application/msword",
        "xls"  => "application/vnd.ms-excel",
        "ppt"  => "application/vnd.ms-powerpoint",
        "epub" => "application/epub+zip",
        "rtf"  => "application/rtf",
        "zip"  => "application/zip",
        "tar"  => "application/x-tar",
        "gz"   => "application/gzip",
        "rar"  => "application/vnd.rar",
        "7z"   => "application/x-7z-compressed",
        "html" => "text/html",    "xml"  => "text/xml",
        "json" => "application/json", "txt" => "text/plain",
        "csv"  => "text/csv",     "md"   => "text/markdown",
        "usdz" => "model/vnd.usdz+zip", "glb" => "model/gltf-binary",
        "gltf" => "model/gltf+json",    "obj" => "model/obj",
        "stl"  => "model/stl",
        _      => "application/octet-stream",
    }
}
