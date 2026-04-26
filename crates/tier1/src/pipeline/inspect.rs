//! Pipeline step: **inspect** — sniff file type and determine processing tier.

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

    let (kind, mime, extension) = sniff(&prefix, &cook.spec.url);
    cook.response.mime = Some(mime);
    cook.response.kind = Some(kind);
    cook.response.extension = Some(extension.to_string());

    let route = dispatch::route(kind, Some(extension));
    cook.trace.job_tier = route.tier;
}

// ── Sniffing ──────────────────────────────────────────────────────────────────

/// Identify the (kind, mime, extension) triple for a byte prefix.
///
/// Magic bytes take priority.  When `infer` returns a generic container type
/// (ZIP, binary), the URL extension is used to refine the kind — DOCX, USDZ,
/// and similar formats are ZIP internally but should surface as their real
/// type.  Falls back to the URL extension alone when magic bytes produce
/// nothing, then to Unknown.
fn sniff(bytes: &[u8], url: &str) -> (FileKind, String, &'static str) {
    let url_ext = url_extension(url);

    if let Some(t) = infer::get(bytes) {
        let infer_ext = canonical_extension(t.extension());
        let infer_kind = mime_to_kind(t.mime_type(), infer_ext);

        // When magic bytes identify a generic container, prefer a more
        // specific kind from the URL extension (e.g. USDZ, DOCX are ZIP).
        if matches!(infer_kind, FileKind::Archive | FileKind::Binary | FileKind::Unknown) {
            if let Some(ext) = url_ext {
                let url_kind = ext_to_kind(ext);
                if !matches!(url_kind, FileKind::Archive | FileKind::Binary | FileKind::Unknown) {
                    return (url_kind, ext_to_mime(ext).to_string(), ext);
                }
            }
        }

        return (infer_kind, t.mime_type().to_string(), infer_ext);
    }

    if let Some(ext) = url_ext {
        return (ext_to_kind(ext), ext_to_mime(ext).to_string(), ext);
    }

    (FileKind::Unknown, "application/octet-stream".to_string(), "bin")
}

// ── Extension helpers ─────────────────────────────────────────────────────────

/// Normalise an `infer`-returned extension to its canonical form.
fn canonical_extension(raw: &str) -> &'static str {
    match raw {
        "jpg"          => "jpeg",
        "tif"          => "tiff",
        "htm"          => "html",
        "mpg"          => "mpeg",
        "mid"          => "midi",
        "svg" | "svgz" => "svg",
        other          => extension_static(other),
    }
}

/// Intern a known extension string to a `&'static str`.
/// Unknown extensions fall back to `"bin"`.
fn extension_static(s: &str) -> &'static str {
    match s {
        "jpeg" => "jpeg", "png"  => "png",  "gif"  => "gif",  "webp" => "webp",
        "bmp"  => "bmp",  "tiff" => "tiff", "avif" => "avif", "heic" => "heic",
        "heif" => "heif", "exr"  => "exr",  "hdr"  => "hdr",  "dng"  => "dng",
        "svg"  => "svg",  "pdf"  => "pdf",  "mp4"  => "mp4",  "mov"  => "mov",
        "mkv"  => "mkv",  "avi"  => "avi",  "webm" => "webm", "mpeg" => "mpeg",
        "mp3"  => "mp3",  "ogg"  => "ogg",  "flac" => "flac", "wav"  => "wav",
        "m4a"  => "m4a",  "zip"  => "zip",  "tar"  => "tar",  "gz"   => "gz",
        "bz2"  => "bz2",  "xz"   => "xz",   "rar"  => "rar",  "7z"   => "7z",
        "html" => "html", "xml"  => "xml",  "json" => "json", "txt"  => "txt",
        "csv"  => "csv",  "md"   => "md",   "docx" => "docx", "xlsx" => "xlsx",
        "pptx" => "pptx", "odt"  => "odt",  "usdz" => "usdz", "glb"  => "glb",
        "gltf" => "gltf", "obj"  => "obj",  "stl"  => "stl",  "emf"  => "emf",
        _      => "bin",
    }
}

/// Extract the last path segment extension from a URL (no dot, lowercased).
fn url_extension(url: &str) -> Option<&'static str> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    let last = path.rsplit('/').next().unwrap_or("");
    let raw  = last.rsplit('.').next().filter(|e| !e.is_empty() && *e != last)?;
    let ext  = extension_static(&raw.to_ascii_lowercase());
    if ext == "bin" { None } else { Some(ext) }
}

// ── Kind / MIME tables ────────────────────────────────────────────────────────

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
        | "application/vnd.oasis.opendocument.text" => FileKind::Document,
        "application/zip"
        | "application/x-tar"
        | "application/gzip"
        | "application/x-bzip2"
        | "application/x-xz"
        | "application/vnd.rar"
        | "application/x-7z-compressed" => FileKind::Archive,
        _ => ext_to_kind(ext),
    }
}

fn ext_to_kind(ext: &str) -> FileKind {
    match ext {
        "jpeg" | "png" | "gif" | "webp" | "bmp" | "tiff"
        | "avif" | "heic" | "heif" | "exr" | "hdr" | "dng" => FileKind::Image,
        "svg"                                               => FileKind::Vector,
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "mpeg"   => FileKind::Video,
        "mp3" | "ogg" | "flac" | "wav" | "m4a"            => FileKind::Audio,
        "pdf" | "docx" | "xlsx" | "pptx" | "odt"          => FileKind::Document,
        "usdz" | "glb" | "gltf" | "obj" | "stl"           => FileKind::Geometry,
        "zip" | "tar" | "gz" | "bz2" | "xz" | "rar" | "7z" => FileKind::Archive,
        "html" | "xml" | "json" | "txt" | "csv" | "md"    => FileKind::Text,
        _                                                  => FileKind::Unknown,
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
        "mp4"  => "video/mp4",    "mov"  => "video/quicktime",
        "mkv"  => "video/x-matroska", "avi" => "video/x-msvideo",
        "webm" => "video/webm",   "mpeg" => "video/mpeg",
        "mp3"  => "audio/mpeg",   "ogg"  => "audio/ogg",
        "flac" => "audio/flac",   "wav"  => "audio/wav",
        "m4a"  => "audio/mp4",
        "pdf"  => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt"  => "application/vnd.oasis.opendocument.text",
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
