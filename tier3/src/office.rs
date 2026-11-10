//! Office document thumbnails via LibreOffice (`soffice --headless`).
//!
//! Covers OpenDocument (odt/ods/odp/odg), Microsoft OOXML (docx/xlsx/pptx),
//! legacy Microsoft binary files (doc/xls/ppt, which the pipeline aliases to
//! docx/xlsx/pptx) and rtf.
//!
//! Embedded raster thumbnails are normally handled by the tier 1 ZIP shortcut
//! before a file ever reaches this module.  What lands here is everything
//! else, handled in this order:
//!
//! 1. An embedded raster thumbnail the shortcut could not reach (decoded
//!    directly, no subprocess).
//! 2. An embedded EMF/WMF thumbnail (common in Microsoft Office files).  No
//!    in-process decoder exists, so the metafile is extracted and handed to
//!    soffice, which renders it as a drawing.
//! 3. The document itself, passed to soffice as-is.
//!
//! Document metadata (the `pages` property) is always scraped from the
//! original document, never from whatever ends up being rendered.

use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::process::{Command, Stdio};

use image::DynamicImage;
use tier1::renderer::RenderOutput;

use crate::scratch::ScratchArena;

/// Canonical extensions routed to the soffice handler.  The pipeline aliases
/// doc/xls/ppt to docx/xlsx/pptx before dispatch, so those are listed here.
pub const OFFICE_EXTENSIONS: &[&str] =
    &["odt", "ods", "odp", "odg", "docx", "xlsx", "pptx", "rtf", "pages", "numbers", "key",
      "xlsm", "xlsb", "xltx", "xltm", "xlt", "xlm", "xlw", "emf", "wmf"];

/// Name under which the handler is registered in the environment check.
pub const HANDLER_NAME: &str = "soffice";

/// Oldest LibreOffice (major, minor) the `--headless --convert-to` pipeline
/// here is known to work with.  Older builds either lack `--convert-to` or
/// predate the current headless render path.
const MIN_LIBREOFFICE_VERSION: (u32, u32) = (6, 0);

/// Largest document staged for conversion.
const MAX_DOCUMENT_BYTES: u64 = 100 * 1024 * 1024;

/// Cap on any single archive member we decompress for inspection, which
/// bounds the damage a zip bomb can do.
const MAX_MEMBER_BYTES: u64 = 32 * 1024 * 1024;

/// Raster thumbnail paths in ODF and OOXML packages, in preference order.
const RASTER_THUMBNAILS: &[&str] = &[
    "Thumbnails/thumbnail.png",
    "docProps/thumbnail.jpeg",
    "docProps/thumbnail.jpg",
    "docProps/thumbnail.png",
];

/// Metafile thumbnail paths that only soffice can render.
const VECTOR_THUMBNAILS: &[(&str, &str)] =
    &[("docProps/thumbnail.emf", "emf"), ("docProps/thumbnail.wmf", "wmf")];

const OLE_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// True when `ext` is rendered by this module.
pub fn is_office_extension(ext: &str) -> bool {
    OFFICE_EXTENSIONS.contains(&ext)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Zip,
    Ole,
    Other,
}

/// Identify the container from magic bytes.  The extension cannot be trusted
/// because the pipeline folds legacy `.doc` into `docx`, and so on.
fn sniff_container(bytes: &[u8]) -> Container {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        Container::Zip
    } else if bytes.starts_with(OLE_MAGIC) {
        Container::Ole
    } else {
        Container::Other
    }
}

/// File name soffice is given.  Legacy binary files get their legacy
/// extension back so the import filter is chosen the way the user's file
/// would have been.
fn staged_name(ext: &str, container: Container) -> String {
    let ext = match (container, ext) {
        (Container::Ole, "docx") => "doc",
        (Container::Ole, "xlsx") => "xls",
        (Container::Ole, "pptx") => "ppt",
        (_, other) => other,
    };
    format!("input.{ext}")
}

/// An EMF/WMF preview: extension plus raw bytes.
type MetafileThumbnail = (&'static str, Vec<u8>);

fn read_member<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let file = archive.by_name(name).ok()?;
    if file.size() > MAX_MEMBER_BYTES {
        return None;
    }
    let mut out = Vec::new();
    file.take(MAX_MEMBER_BYTES).read_to_end(&mut out).ok()?;
    Some(out)
}

/// Find an embedded thumbnail in a ZIP package.  Raster images win over
/// metafiles since they need no subprocess.
fn find_embedded_thumbnails(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
) -> (Option<Vec<u8>>, Option<MetafileThumbnail>) {
    let mut raster = None;
    for name in RASTER_THUMBNAILS {
        if let Some(data) = read_member(archive, name) {
            raster = Some(data);
            break;
        }
    }
    let mut vector = None;
    for (name, ext) in VECTOR_THUMBNAILS {
        if let Some(data) = read_member(archive, name) {
            vector = Some((*ext, data));
            break;
        }
    }
    (raster, vector)
}

//  Metadata scraping

/// Page (or slide) count from an ODF `meta.xml`.
/// Spreadsheets report their sheet count (`table-count`) instead.
fn odf_pages(xml: &[u8], ext: &str) -> Option<u32> {
    let doc = roxmltree::Document::parse(std::str::from_utf8(xml).ok()?).ok()?;
    let meta_ns = "urn:oasis:names:tc:opendocument:xmlns:meta:1.0";
    let stat = doc
        .descendants()
        .find(|n| n.has_tag_name((meta_ns, "document-statistic")))?;
    let attr = if ext == "ods" { "table-count" } else { "page-count" };
    positive(stat.attribute((meta_ns, attr))?)
}

/// Worksheet count from the "Worksheets" entry of `docProps/app.xml`
/// HeadingPairs, which alternates a lpstr label with an i4 count.
fn xlsx_app_sheets(xml: &[u8]) -> Option<u32> {
    let doc = roxmltree::Document::parse(std::str::from_utf8(xml).ok()?).ok()?;
    let pairs = doc.descendants().find(|n| n.tag_name().name() == "HeadingPairs")?;
    let mut values = pairs.descendants().filter(|n| n.tag_name().name() == "variant");
    while let Some(label) = values.next() {
        let count = values.next()?;
        let text = |n: roxmltree::Node| {
            n.descendants().find(|c| c.is_text()).and_then(|c| c.text()).map(|t| t.trim().to_string())
        };
        if text(label).as_deref() == Some("Worksheets") {
            return text(count).as_deref().and_then(positive);
        }
    }
    None
}

/// Visible sheet count from an OOXML `xl/workbook.xml`.
fn xlsx_workbook_sheets(xml: &[u8]) -> Option<u32> {
    let doc = roxmltree::Document::parse(std::str::from_utf8(xml).ok()?).ok()?;
    let n = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "sheet")
        .filter(|n| n.attribute("state").is_none_or(|s| s == "visible"))
        .count();
    positive(&n.to_string())
}

/// Page or slide count from an OOXML `docProps/app.xml`.  Slides take
/// precedence for presentations; spreadsheets carry neither.
fn ooxml_pages(xml: &[u8], prefer_slides: bool) -> Option<u32> {
    let doc = roxmltree::Document::parse(std::str::from_utf8(xml).ok()?).ok()?;
    let ns = "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties";
    let find = |name: &str| {
        doc.descendants()
            .find(|n| n.has_tag_name((ns, name)))
            .and_then(|n| n.text())
            .and_then(positive)
    };
    if prefer_slides {
        find("Slides").or_else(|| find("Pages"))
    } else {
        find("Pages").or_else(|| find("Slides"))
    }
}

fn positive(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|n| *n > 0)
}

fn zip_pages(archive: &mut zip::ZipArchive<Cursor<&[u8]>>, ext: &str) -> Option<u32> {
    if let Some(meta) = read_member(archive, "meta.xml") {
        return odf_pages(&meta, ext);
    }
    if matches!(ext, "xlsx" | "xlsm" | "xlsb" | "xltx" | "xltm") {
        return read_member(archive, "docProps/app.xml")
            .and_then(|app| xlsx_app_sheets(&app))
            .or_else(|| read_member(archive, "xl/workbook.xml").and_then(|wb| xlsx_workbook_sheets(&wb)));
    }
    let app = read_member(archive, "docProps/app.xml")?;
    ooxml_pages(&app, ext == "pptx")
}

/// Read a numeric property from the first section of an OLE property-set
/// stream (`\x05SummaryInformation` and friends).
fn property_set_u32(stream: &[u8], property_id: u32) -> Option<u32> {
    let u32_at = |off: usize| -> Option<u32> {
        stream.get(off..off.checked_add(4)?).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    // Header: byte order, version, os, CLSID (16), section count; then
    // (FMTID(16), offset) per section.
    if u32_at(24)? == 0 {
        return None;
    }
    let section = u32_at(44)? as usize;
    let count = u32_at(section.checked_add(4)?)? as usize;
    for i in 0..count.min(1024) {
        let entry = section.checked_add(8)?.checked_add(i.checked_mul(8)?)?;
        if u32_at(entry)? != property_id {
            continue;
        }
        let value = section.checked_add(u32_at(entry + 4)? as usize)?;
        // VT_I4 or VT_UI4
        return match u32_at(value)? & 0xFFFF {
            3 | 19 => u32_at(value + 4),
            _ => None,
        };
    }
    None
}

/// Page count of a legacy binary document: PIDSI_PAGECOUNT from the summary
/// information, or PIDDSI_SLIDECOUNT from the document summary for decks.
fn ole_pages(bytes: &[u8], ext: &str) -> Option<u32> {
    let mut comp = cfb::CompoundFile::open(Cursor::new(bytes)).ok()?;
    if matches!(ext, "xlsx" | "xlt" | "xlm" | "xlw") {
        let mut stream = comp.open_stream("/Workbook").or_else(|_| comp.open_stream("/Book")).ok()?;
        let mut data = Vec::new();
        stream.by_ref().take(MAX_MEMBER_BYTES).read_to_end(&mut data).ok()?;
        return xls_sheets(&data);
    }
    let mut read_stream = |path: &str| -> Option<Vec<u8>> {
        let mut s = comp.open_stream(path).ok()?;
        let mut out = Vec::new();
        s.by_ref().take(MAX_MEMBER_BYTES).read_to_end(&mut out).ok()?;
        Some(out)
    };
    let pages = read_stream("/\u{5}SummaryInformation")
        .and_then(|s| property_set_u32(&s, 14))
        .filter(|n| *n > 0);
    let mut slides = || {
        read_stream("/\u{5}DocumentSummaryInformation")
            .and_then(|s| property_set_u32(&s, 7))
            .filter(|n| *n > 0)
    };
    if ext == "pptx" { slides().or(pages) } else { pages.or_else(slides) }
}

/// Count visible worksheets and chart sheets from the BOUNDSHEET records in
/// the globals substream of a BIFF8 workbook.
fn xls_sheets(data: &[u8]) -> Option<u32> {
    let mut pos = 0usize;
    let mut count = 0u32;
    while let Some(head) = data.get(pos..pos + 4) {
        let id = u16::from_le_bytes([head[0], head[1]]);
        let len = u16::from_le_bytes([head[2], head[3]]) as usize;
        let body = data.get(pos + 4..pos + 4 + len)?;
        match id {
            // BOUNDSHEET: offset u32, state u8, type u8
            0x0085 if body.len() >= 6 && body[4] == 0 && matches!(body[5], 0 | 2) => count += 1,
            // EOF ends the globals substream
            0x000A => break,
            _ => {}
        }
        pos += 4 + len;
    }
    positive(&count.to_string())
}

fn scrape_pages(
    bytes: &[u8],
    container: Container,
    archive: Option<&mut zip::ZipArchive<Cursor<&[u8]>>>,
    ext: &str,
) -> Option<u32> {
    if ext == "odg" {
        return None;
    }
    match container {
        Container::Zip => zip_pages(archive?, ext),
        Container::Ole => ole_pages(bytes, ext),
        Container::Other => None,
    }
}

//  soffice

/// Probe for a `soffice` build that can actually do what this module needs.
///
/// Finding `soffice` on `$PATH` is not enough: Apache OpenOffice and
/// StarOffice also ship a `soffice` wrapper but use a different command line
/// and do not implement LibreOffice's `--convert-to` pipeline, and LibreOffice
/// builds older than [`MIN_LIBREOFFICE_VERSION`] lack the arguments used here.
///
/// The probe only runs `soffice --version`: the banner identifies the build as
/// **LibreOffice** (rejecting OpenOffice/StarOffice shims) and yields the
/// version, which must be at least [`MIN_LIBREOFFICE_VERSION`].  `--headless`
/// and `--convert-to` have existed since long before that minimum, so parsing
/// `--help` would tell us nothing extra - and it is slow, since it can spin up
/// a session just to print usage.
///
/// Returns `(available, details, unavailable_reason)` in the same shape as the
/// other tier-3 probes.
pub fn probe() -> (bool, Option<String>, Option<String>) {
    //  Identify the build from its version banner.
    let mut cmd = Command::new("soffice");
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match crate::sandbox::output(&mut cmd, &crate::sandbox::probe()) {
        Ok(output) => output,
        Err(e) => return (false, None, Some(format!("soffice --version: {e}"))),
    };
    if output.status.code() != Some(0) {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return (
            false,
            None,
            Some(format!("soffice --version exit {:?}: {detail}", output.status.code())),
        );
    }
    let mut banner = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if banner.is_empty() {
        banner = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }

    let first_line = banner.lines().next().unwrap_or("").trim().to_string();
    let label = format!("soffice: {first_line}");

    if let Err(reason) = check_banner(&banner) {
        return (false, Some(label), Some(reason));
    }

    (true, Some(label), None)
}

/// Validate a `soffice --version` banner, returning the parsed
/// `(major, minor)` version or a human-readable rejection reason.
fn check_banner(banner: &str) -> Result<(u32, u32), String> {
    let first_line = banner.lines().next().unwrap_or("").trim();

    if !banner.contains("LibreOffice") {
        return Err(if first_line.is_empty() {
            "soffice --version produced no output (not LibreOffice?)".into()
        } else {
            format!(
                "not LibreOffice ({first_line}); OpenOffice/StarOffice lack --convert-to"
            )
        });
    }

    let version = parse_libreoffice_version(banner).ok_or_else(|| {
        format!("could not parse a LibreOffice version from '{first_line}'")
    })?;

    if version < MIN_LIBREOFFICE_VERSION {
        return Err(format!(
            "LibreOffice {}.{} is too old (need {}.{}+ for --headless --convert-to)",
            version.0, version.1, MIN_LIBREOFFICE_VERSION.0, MIN_LIBREOFFICE_VERSION.1,
        ));
    }

    Ok(version)
}

/// Parse `(major, minor)` from a banner such as
/// `LibreOffice 7.4.7.2 40(Build:2)` or `LibreOffice 24.8.4.2 48(Build:2)`.
fn parse_libreoffice_version(banner: &str) -> Option<(u32, u32)> {
    let rest = banner.split_once("LibreOffice")?.1.trim_start();
    let token = rest.split_whitespace().next()?;
    let mut parts = token.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Convert `input` to a PNG with soffice and decode the result.
///
/// Every run gets its own profile directory inside the arena (a shared
/// profile would serialise concurrent conversions on a lock file) and a
/// scratch `HOME`, so nothing escapes the arena.
fn soffice_png(arena: &ScratchArena, input: &Path) -> Result<DynamicImage, String> {
    let out_dir = arena.output_path("d");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create output dir: {e}"))?;
    let profile = arena.root().join("lo_profile");

    let mut cmd = Command::new("soffice");
    cmd.arg("--headless")
        .arg("--invisible")
        .arg("--norestore")
        .arg("--nolockcheck")
        .arg("--nodefault")
        .arg("--nologo")
        .arg("--nofirststartwizard")
        .arg(format!("-env:UserInstallation=file://{}", profile.display()))
        .arg("--convert-to")
        .arg("png")
        .arg("--outdir")
        .arg(&out_dir)
        .arg(input)
        .env("HOME", arena.root())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = crate::sandbox::output(&mut cmd, &crate::sandbox::office())
        .map_err(|e| format!("run soffice: {e}"))?;

    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("input");
    let png_path = out_dir.join(format!("{stem}.png"));

    // soffice can exit 0 without writing anything ("source file could not be
    // loaded"), so the output file is the real success signal.
    if !output.status.success() || !png_path.exists() {
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if detail.is_empty() {
            detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
        let detail = detail.lines().next().unwrap_or("no output").to_string();
        return Err(format!("soffice exited with {}: {detail}", output.status));
    }

    let png = arena.read_output(&png_path).map_err(|e| format!("read output: {e}"))?;
    image::load_from_memory(&png).map_err(|e| format!("decode PNG output: {e}"))
}

/// Pixel density of soffice's PNG export.
const EXPORT_DPI: f64 = 96.0;

/// Largest frame (mm) a metafile is scaled to.  Fits inside an A4 page, the
/// page size soffice's drawing export always produces.
const FRAME_MAX_MM: (f64, f64) = (200.0, 287.0);

/// Physical size in mm of an EMF or WMF, from its header.
fn metafile_size_mm(ext: &str, data: &[u8]) -> Option<(f64, f64)> {
    let (w, h) = match ext {
        "emf" => {
            // ENHMETAHEADER: record type 1, rclFrame in 0.01 mm at offset 24,
            // " EMF" signature at offset 40.
            if data.get(0..4)? != [1, 0, 0, 0] || data.get(40..44)? != b" EMF" {
                return None;
            }
            let i32_at = |off: usize| -> Option<i32> {
                Some(i32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
            };
            (f64::from(i32_at(32)? - i32_at(24)?) / 100.0, f64::from(i32_at(36)? - i32_at(28)?) / 100.0)
        }
        "wmf" => {
            // Placeable header: magic, handle, bounding box (i16 x4), units
            // per inch.  Plain WMFs carry no size at all.
            if data.get(0..4)? != [0xD7, 0xCD, 0xC6, 0x9A] {
                return None;
            }
            let i16_at = |off: usize| -> Option<i16> {
                Some(i16::from_le_bytes(data.get(off..off + 2)?.try_into().ok()?))
            };
            let inch = f64::from(u16::from_le_bytes(data.get(14..16)?.try_into().ok()?));
            if inch == 0.0 {
                return None;
            }
            (
                f64::from(i16_at(10)?.checked_sub(i16_at(6)?)?) / inch * 25.4,
                f64::from(i16_at(12)?.checked_sub(i16_at(8)?)?) / inch * 25.4,
            )
        }
        _ => return None,
    };
    (w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()).then_some((w, h))
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Wrap a metafile in a one-page flat ODG.
///
/// Opened directly, soffice drops an EMF at its natural size in the middle of
/// an A4 page, and the PNG export is always the whole page.  Instead the
/// metafile is placed at the page origin, scaled up (it is vector) to fill as
/// much of the page as its aspect ratio allows, and the caller crops the PNG
/// to the returned pixel size.
fn wrap_metafile(ext: &str, data: &[u8]) -> Option<(String, u32, u32)> {
    let (w, h) = metafile_size_mm(ext, data)?;
    let scale = (FRAME_MAX_MM.0 / w).min(FRAME_MAX_MM.1 / h);
    let (w, h) = (w * scale, h * scale);
    let fodg = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" office:version="1.3" office:mimetype="application/vnd.oasis.opendocument.graphics"><office:body><office:drawing><draw:page draw:name="thumbnail"><draw:frame svg:x="0mm" svg:y="0mm" svg:width="{w:.3}mm" svg:height="{h:.3}mm"><draw:image><office:binary-data>{}</office:binary-data></draw:image></draw:frame></draw:page></office:drawing></office:body></office:document>"#,
        base64(data)
    );
    let px = |mm: f64| ((mm / 25.4 * EXPORT_DPI).floor() as u32).max(1);
    Some((fodg, px(w), px(h)))
}

/// Render an EMF/WMF thumbnail through soffice, cropped to the picture.
fn render_metafile(arena: &ScratchArena, ext: &str, data: &[u8]) -> Result<DynamicImage, String> {
    let (fodg, w, h) = wrap_metafile(ext, data).ok_or("unrecognised metafile header")?;
    let staged = arena
        .stage_bytes(fodg.as_bytes(), "thumbnail.fodg")
        .map_err(|e| format!("stage thumbnail: {e}"))?;
    let page = soffice_png(arena, &staged)?;
    Ok(page.crop_imm(0, 0, w.min(page.width()), h.min(page.height())))
}

fn output(image: DynamicImage, renderer: &str, pages: Option<u32>) -> RenderOutput {
    RenderOutput {
        image,
        renderer: Some(renderer.into()),
        codec: None,
        video_seek_secs: None,
        properties: pages.map(|n| serde_json::json!({ "pages": n })),
    }
}

/// Render the first page of an office document.  `ext` is the pipeline's
/// canonical extension; the container type is sniffed from the bytes.
pub fn run_office_handler(
    reader: &mut dyn tier1::ReadSeek,
    ext: &str,
) -> Result<RenderOutput, String> {
    reader.seek(std::io::SeekFrom::Start(0)).map_err(|e| format!("rewind input: {e}"))?;
    let mut bytes = Vec::new();
    reader
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read input: {e}"))?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(format!("document larger than {} MiB", MAX_DOCUMENT_BYTES >> 20));
    }

    let container = sniff_container(&bytes);
    let mut archive = match container {
        Container::Zip => zip::ZipArchive::new(Cursor::new(&bytes[..])).ok(),
        _ => None,
    };
    let pages = scrape_pages(&bytes, container, archive.as_mut(), ext);

    let arena = ScratchArena::new(2 * MAX_DOCUMENT_BYTES).map_err(|e| format!("scratch arena: {e}"))?;

    if matches!(ext, "emf" | "wmf") {
        let image = if metafile_size_mm(ext, &bytes).is_some() {
            render_metafile(&arena, ext, &bytes)?
        } else {
            let staged = arena.stage_bytes(&bytes, &staged_name(ext, container))
                .map_err(|e| format!("stage input: {e}"))?;
            soffice_png(&arena, &staged)?
        };
        return Ok(output(image, "soffice", None));
    }

    if let Some(archive) = archive.as_mut() {
        let (raster, vector) = find_embedded_thumbnails(archive);
        if let Some(img) = raster.and_then(|data| image::load_from_memory(&data).ok()) {
            return Ok(output(img, "office/thumbnail", pages));
        }
        if let Some((meta_ext, data)) = vector {
            match render_metafile(&arena, meta_ext, &data) {
                Ok(img) => return Ok(output(img, "soffice/thumbnail", pages)),
                // A broken placeholder should not sink the whole render.
                Err(e) => {
                    if tier1::ux::show_raw_logs() {
                        eprintln!("[tier3] soffice {meta_ext} thumbnail: {e}");
                    }
                }
            }
        }
    }

    let staged = arena
        .stage_bytes(&bytes, &staged_name(ext, container))
        .map_err(|e| format!("stage input: {e}"))?;
    let img = soffice_png(&arena, &staged)?;
    Ok(output(img, "soffice", pages))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn build_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn metafile_sizes_come_from_headers() {
        let mut emf = vec![0u8; 100];
        emf[0] = 1;
        emf[24..28].copy_from_slice(&0i32.to_le_bytes());
        emf[28..32].copy_from_slice(&0i32.to_le_bytes());
        emf[32..36].copy_from_slice(&10000i32.to_le_bytes()); // 100 mm
        emf[36..40].copy_from_slice(&5000i32.to_le_bytes()); // 50 mm
        emf[40..44].copy_from_slice(b" EMF");
        assert_eq!(metafile_size_mm("emf", &emf), Some((100.0, 50.0)));
        let (fodg, w, h) = wrap_metafile("emf", &emf).unwrap();
        // Scaled to the 200 mm frame limit: 756 x 378 px at 96 dpi.
        assert_eq!((w, h), (755, 377));
        assert!(fodg.contains("svg:width=\"200.000mm\""));
        emf[40] = b'X';
        assert_eq!(metafile_size_mm("emf", &emf), None);

        let mut wmf = vec![0u8; 22];
        wmf[0..4].copy_from_slice(&[0xD7, 0xCD, 0xC6, 0x9A]);
        wmf[10..12].copy_from_slice(&1440i16.to_le_bytes()); // right
        wmf[12..14].copy_from_slice(&720i16.to_le_bytes()); // bottom
        wmf[14..16].copy_from_slice(&1440u16.to_le_bytes()); // units per inch
        let (w, h) = metafile_size_mm("wmf", &wmf).unwrap();
        assert!((w - 25.4).abs() < 1e-9 && (h - 12.7).abs() < 1e-9);
        assert_eq!(metafile_size_mm("wmf", b"\x01\x00\x09\x00"), None);
    }

    #[test]
    fn base64_matches_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn container_is_sniffed_from_bytes() {
        assert_eq!(sniff_container(b"PK\x03\x04rest"), Container::Zip);
        assert_eq!(sniff_container(OLE_MAGIC), Container::Ole);
        assert_eq!(sniff_container(b"{\\rtf1"), Container::Other);
    }

    #[test]
    fn legacy_files_regain_their_extension() {
        assert_eq!(staged_name("docx", Container::Ole), "input.doc");
        assert_eq!(staged_name("xlsx", Container::Ole), "input.xls");
        assert_eq!(staged_name("pptx", Container::Ole), "input.ppt");
        assert_eq!(staged_name("docx", Container::Zip), "input.docx");
        assert_eq!(staged_name("rtf", Container::Other), "input.rtf");
    }

    #[test]
    fn odf_and_ooxml_page_counts() {
        let odf = br#"<m xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"><meta:document-statistic meta:page-count="12"/></m>"#;
        assert_eq!(odf_pages(odf, "odt"), Some(12));
        let app = br#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Pages>7</Pages><Slides>4</Slides></Properties>"#;
        assert_eq!(ooxml_pages(app, false), Some(7));
        assert_eq!(ooxml_pages(app, true), Some(4));
        let zero = br#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Pages>0</Pages></Properties>"#;
        assert_eq!(ooxml_pages(zero, false), None);
    }

    #[test]
    fn spreadsheet_sheet_counts() {
        let ods = br#"<m xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0"><meta:document-statistic meta:table-count="3" meta:page-count="9"/></m>"#;
        assert_eq!(odf_pages(ods, "ods"), Some(3));
        let app = br#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><HeadingPairs><vt:vector size="4" baseType="variant"><vt:variant><vt:lpstr>Worksheets</vt:lpstr></vt:variant><vt:variant><vt:i4>5</vt:i4></vt:variant><vt:variant><vt:lpstr>Named Ranges</vt:lpstr></vt:variant><vt:variant><vt:i4>2</vt:i4></vt:variant></vt:vector></HeadingPairs></Properties>"#;
        assert_eq!(xlsx_app_sheets(app), Some(5));
        let wb = br#"<workbook><sheets><sheet name="a"/><sheet name="b" state="hidden"/><sheet name="c"/></sheets></workbook>"#;
        assert_eq!(xlsx_workbook_sheets(wb), Some(2));
        let mut xls = Vec::new();
        for (state, kind) in [(0u8, 0u8), (1, 0), (0, 2)] {
            xls.extend_from_slice(&[0x85, 0x00, 8, 0, 0, 0, 0, 0, state, kind, 1, 0]);
        }
        xls.extend_from_slice(&[0x0A, 0, 0, 0]);
        assert_eq!(xls_sheets(&xls), Some(2));
    }

    #[test]
    fn thumbnails_are_split_into_raster_and_metafile() {
        let bytes = build_zip(&[
            ("docProps/app.xml", b"<x/>"),
            ("docProps/thumbnail.emf", b"EMF"),
        ]);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        let (raster, vector) = find_embedded_thumbnails(&mut archive);
        assert!(raster.is_none());
        assert!(matches!(vector, Some(("emf", _))));
    }

    #[test]
    fn zip_pages_reads_metadata_even_with_a_metafile_thumbnail() {
        let app = br#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Pages>3</Pages></Properties>"#;
        let bytes = build_zip(&[("docProps/app.xml", app), ("docProps/thumbnail.emf", b"EMF")]);
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes[..])).unwrap();
        assert_eq!(zip_pages(&mut archive, "docx"), Some(3));
    }

    #[test]
    fn property_set_reads_i4_values() {
        // header(28) + one section descriptor(20) + section at 48.
        let mut s = vec![0u8; 48];
        s[24..28].copy_from_slice(&1u32.to_le_bytes());
        s[44..48].copy_from_slice(&48u32.to_le_bytes());
        let mut section = Vec::new();
        section.extend_from_slice(&0u32.to_le_bytes()); // size (unused)
        section.extend_from_slice(&1u32.to_le_bytes()); // one property
        section.extend_from_slice(&14u32.to_le_bytes()); // id
        section.extend_from_slice(&16u32.to_le_bytes()); // offset in section
        section.extend_from_slice(&3u32.to_le_bytes()); // VT_I4
        section.extend_from_slice(&9u32.to_le_bytes());
        s.extend(section);
        assert_eq!(property_set_u32(&s, 14), Some(9));
        assert_eq!(property_set_u32(&s, 7), None);
        assert_eq!(property_set_u32(&s[..30], 14), None);
    }

    #[test]
    fn parses_libreoffice_version() {
        assert_eq!(
            parse_libreoffice_version("LibreOffice 7.4.7.2 40(Build:2)"),
            Some((7, 4))
        );
        assert_eq!(
            parse_libreoffice_version("LibreOffice 24.8.4.2 48(Build:2)"),
            Some((24, 8))
        );
        // No minor component degrades to 0.
        assert_eq!(parse_libreoffice_version("LibreOffice 7"), Some((7, 0)));
        assert_eq!(parse_libreoffice_version("OpenOffice 4.1.14"), None);
        assert_eq!(parse_libreoffice_version(""), None);
    }

    #[test]
    fn rejects_non_libreoffice_and_old_builds() {
        // A genuine LibreOffice banner above the minimum passes.
        assert!(check_banner("LibreOffice 7.4.7.2 40(Build:2)\n").is_ok());
        // OpenOffice/StarOffice ship a soffice shim but no --convert-to.
        assert!(check_banner("Apache OpenOffice 4.1.14\n").is_err());
        assert!(check_banner("StarOffice 9\n").is_err());
        // Empty banner (broken wrapper).
        assert!(check_banner("").is_err());
        // Old LibreOffice is rejected by version, not just identity.
        assert!(check_banner("LibreOffice 5.4.7.2 40(Build:2)\n").is_err());
    }
}
