//! End-to-end tests for the soffice office handler.  They generate their own
//! fixtures with soffice and skip (pass) when it is not installed.

use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::process::Command;

use tier3::office::run_office_handler;

#[cfg(unix)]
fn probe_with_slow_soffice(delay_secs: u64, args: &[&str]) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let soffice = dir.path().join("soffice");
    std::fs::write(&soffice, format!("#!/bin/sh\nsleep {delay_secs}\necho 'LibreOffice 24.2.7.2'\n"))
        .unwrap();
    std::fs::set_permissions(&soffice, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_thumbrella"));
    child
        .args(args)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env_remove("TBR_TIER3")
        .env("TBR_LOG", "full");
    if args == ["probe"] {
        child.env("TBR_PROBE", "ffmpeg");
    } else {
        child.env_remove("TBR_PROBE");
    }
    child.output().unwrap()
}

#[cfg(unix)]
#[test]
fn explicit_probe_allows_slow_tools_without_delaying_startup() {
    let startup = probe_with_slow_soffice(4, &["check"]);
    assert!(startup.status.success());
    let diagnostics = String::from_utf8_lossy(&startup.stderr);
    let unavailable = diagnostics.split("unavailable (").nth(1).expect("probe summary");
    assert!(unavailable.split(')').next().unwrap().split(", ").any(|name| name == "soffice"));

    let probe = probe_with_slow_soffice(4, &["probe"]);
    assert!(probe.status.success());
    let output = String::from_utf8_lossy(&probe.stdout);
    assert!(output.trim().strip_prefix("TBR_PROBE=").unwrap().split(',').any(|name| name == "soffice"));

    let result = probe_with_slow_soffice(4, &["result", "probe"]);
    assert!(result.status.success());
    let diagnostics = String::from_utf8_lossy(&result.stderr);
    let unavailable = diagnostics.split("unavailable (").nth(1).expect("probe summary");
    assert!(unavailable.split(')').next().unwrap().split(", ").any(|name| name == "soffice"));
}

#[cfg(unix)]
#[test]
fn explicit_probe_still_times_out_hung_tools() {
    let probe = probe_with_slow_soffice(11, &["probe"]);
    assert!(probe.status.success());
    let output = String::from_utf8_lossy(&probe.stdout);
    assert!(
        !output.trim().strip_prefix("TBR_PROBE=").unwrap().split(',').any(|name| name == "soffice")
    );
}

fn have_soffice() -> bool {
    which::which("soffice").is_ok()
}

fn soffice_convert(input: &Path, format: &str, outdir: &Path) -> std::path::PathBuf {
    let status = Command::new("soffice")
        .args(["--headless", "--convert-to", format, "--outdir"])
        .arg(outdir)
        .arg(input)
        .env("HOME", outdir)
        .output()
        .expect("run soffice");
    assert!(status.status.success(), "soffice convert failed");
    outdir.join(input.file_stem().unwrap()).with_extension(format)
}

fn render(bytes: Vec<u8>, ext: &str) -> tier3::RenderOutput {
    let mut reader = Cursor::new(bytes);
    run_office_handler(&mut reader, ext).expect("render")
}

fn sample_text(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("doc.txt");
    std::fs::write(&path, "Thumbrella office test\n\nSecond paragraph.\n").unwrap();
    path
}

#[test]
fn docx_and_odt_render_as_is() {
    if !have_soffice() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let txt = sample_text(dir.path());
    for format in ["docx", "odt", "rtf", "doc"] {
        let file = soffice_convert(&txt, format, dir.path());
        // The pipeline folds doc into docx, so pass the alias for legacy files.
        let ext = if format == "doc" { "docx" } else { format };
        let out = render(std::fs::read(&file).unwrap(), ext);
        assert!(out.image.width() > 100 && out.image.height() > 100, "{format}");
        // LibreOffice writes its own PNG thumbnail into ODF, which wins.
        let expected = if format == "odt" { "office/thumbnail" } else { "soffice" };
        assert_eq!(out.renderer.as_deref(), Some(expected), "{format}");
    }
}

#[test]
fn metafile_thumbnail_is_rendered_but_pages_come_from_the_document() {
    if !have_soffice() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let svg = dir.path().join("thumb.svg");
    std::fs::write(
        &svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="100"><rect width="256" height="100" fill="#c33"/></svg>"##,
    )
    .unwrap();
    let emf = std::fs::read(soffice_convert(&svg, "emf", dir.path())).unwrap();

    let app = r#"<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"><Pages>42</Pages></Properties>"#;
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("docProps/app.xml", opts).unwrap();
    zip.write_all(app.as_bytes()).unwrap();
    zip.start_file("docProps/thumbnail.emf", opts).unwrap();
    zip.write_all(&emf).unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    // Cropped to the metafile's own frame (EMF header rclFrame), not a page.
    let frame = |off: usize| i32::from_le_bytes(emf[off..off + 4].try_into().unwrap()) as f64;
    let want = (frame(32) - frame(24)) / (frame(36) - frame(28));
    let out = render(bytes, "docx");
    assert_eq!(out.renderer.as_deref(), Some("soffice/thumbnail"));
    let ratio = out.image.width() as f64 / out.image.height() as f64;
    assert!((ratio - want).abs() < 0.05, "aspect {ratio} vs {want}");
    assert_eq!(out.properties, Some(serde_json::json!({ "pages": 42 })));
}

#[test]
fn odg_preview_and_render_are_vectors_without_pages_or_page_edges() {
    if !have_soffice() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let svg = dir.path().join("drawing.svg");
    std::fs::write(
            &svg,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="100"><rect width="256" height="100" fill="#c33"/></svg>"##,
        )
        .unwrap();
    let original = soffice_convert(&svg, "odg", dir.path());
    let original = std::fs::read(original).unwrap();
    let mut paths = Vec::new();
    for (name, preview, pages) in [
        ("preview-single.odg", true, 1),
        ("preview-many.odg", true, 12),
        ("render-single.odg", false, 1),
        ("render-many.odg", false, 12),
    ] {
        let mut archive = zip::ZipArchive::new(Cursor::new(&original)).unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let entry_name = entry.name().to_owned();
            if !preview && entry_name == "Thumbnails/thumbnail.png" {
                continue;
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if entry_name == "meta.xml" {
                bytes = format!(
                        r#"<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" office:version="1.3"><office:meta><meta:document-statistic meta:page-count="{pages}"/></office:meta></office:document-meta>"#
                    )
                    .into_bytes();
            } else if entry_name == "content.xml" && pages > 1 {
                bytes.extend(vec![b' '; 40_000]);
            }
            let options =
                zip::write::SimpleFileOptions::default().compression_method(entry.compression());
            writer.start_file(entry_name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        let path = dir.path().join(name);
        std::fs::write(&path, writer.finish().unwrap().into_inner()).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_thumbrella"))
            .arg("result")
            .arg(&path)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(result.status.success(), "{name}: {}", String::from_utf8_lossy(&result.stderr));
        let result: serde_json::Value = serde_json::from_slice(&result.stdout)
            .unwrap_or_else(|e| panic!("{name}: invalid JSON: {e}: {}", String::from_utf8_lossy(&result.stdout)));
        assert_eq!(result["status"], "success", "{name}: {result}");
        assert_eq!(result["source"], if preview { "shortcut" } else { "render" }, "{name}");
        assert_eq!(result["media"]["kind"], "vector", "{name}");
        assert_eq!(result["media"]["extension"], "odg", "{name}");
        assert!(result["media"]["properties"].get("pages").is_none(), "{name}: {result}");
        paths.push(path);
    }

    let mut thumbnails = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let output = dir.path().join(format!("preview-{index}.jpeg"));
        let status = Command::new(env!("CARGO_BIN_EXE_thumbrella"))
            .arg("thumb")
            .arg(path)
            .arg(&output)
            .status()
            .unwrap();
        assert!(status.success());
        thumbnails.push(std::fs::read(output).unwrap());
    }
    assert_eq!(thumbnails[0], thumbnails[1], "page metadata must not change the preview");
    assert_eq!(thumbnails[2], thumbnails[3], "page metadata must not change the rendered image");
}

#[test]
fn spreadsheet_variants_and_metafiles_render() {
    if !have_soffice() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("sheet.csv");
    std::fs::write(&csv, "Name,Value\nExample,42\n").unwrap();
    for ext in ["xlsm", "xltx", "xltm", "xlt", "xlm", "xlw"] {
        let file = soffice_convert(&csv, ext, dir.path());
        let out = render(std::fs::read(&file).unwrap(), ext);
        assert_eq!(out.renderer.as_deref(), Some("soffice"), "{ext}");
        assert!(out.image.width() > 100 && out.image.height() > 100, "{ext}");
        assert_eq!(out.properties, Some(serde_json::json!({"pages": 1})), "{ext}");
    }

    let svg = dir.path().join("art.svg");
    std::fs::write(
        &svg,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="100"><rect width="256" height="100" fill="#c33"/></svg>"##,
    )
    .unwrap();
    for ext in ["emf", "wmf"] {
        let file = soffice_convert(&svg, ext, dir.path());
        let out = render(std::fs::read(&file).unwrap(), ext);
        assert_eq!(out.renderer.as_deref(), Some("soffice"), "{ext}");
        assert!(out.image.width() > out.image.height(), "{ext}");
        let image = out.image.to_rgb8();
        let center = image.get_pixel(image.width() / 2, image.height() / 2);
        assert!(center[0] > 150 && center[1] < 120, "{ext}: {center:?}");
    }
}
