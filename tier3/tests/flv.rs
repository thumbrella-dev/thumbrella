use std::process::Command;

fn format_entry(path: &std::ffi::OsStr, extension: &str) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_thumbrella"))
        .env_remove("TBR_TIER2")
        .env_remove("TBR_TIER3")
        .env("PATH", path)
        .args(["formats", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    json["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["extensions"].as_array().unwrap())
        .find(|entry| entry["extension"] == extension)
        .unwrap()
        .clone()
}

/// The external ffmpeg handler needs `ffprobe` as well as `ffmpeg`, and DICOM
/// is the one format that still relies on it (everything FLV used to cover is
/// decoded by the builtin libav at tier 2).
#[cfg(unix)]
#[test]
fn ffmpeg_cli_formats_are_unavailable_without_ffprobe() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ffmpeg = dir.path().join("ffmpeg");
    std::fs::write(&ffmpeg, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
    let entry = format_entry(dir.path().as_os_str(), "dcm");
    assert_eq!(entry["tier"], 3);
    assert_eq!(entry["renderer"], "ffmpeg_cli");
    assert_eq!(entry["available"], false);
}

#[test]
#[ignore = "requires an `flv` encoder on PATH to build the fixture"]
fn flv_renders_a_frame_with_the_builtin_libav() {
    let path = std::env::var_os("PATH").unwrap();
    let entry = format_entry(&path, "flv");
    // FLV is a tier-2 format now: the builtin libav decodes flv1.  The tier-3
    // external ffmpeg only matters for codecs that build lacks.
    assert_eq!(entry["tier"], 2);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("video.flv");
    let output = dir.path().join("thumbnail.jpeg");
    let generated = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=128x96:rate=10",
        ])
        .args(["-t", "2", "-c:v", "flv", "-y"])
        .arg(&input)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{}", String::from_utf8_lossy(&generated.stderr));

    let result = Command::new(env!("CARGO_BIN_EXE_thumbrella"))
        .env_remove("TBR_TIER2")
        .env_remove("TBR_TIER3")
        .env("TBR_LOG", "full")
        .args(["thumb"])
        .arg(&input)
        .arg(&output)
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let stderr = String::from_utf8_lossy(&result.stderr);
    // Decoded in-process: no external ffmpeg subprocess was spawned.
    assert!(!stderr.contains("[ffmpeg] exit="), "external ffmpeg was used:\n{stderr}");
    assert!(stderr.contains("[tier2] render"), "tier2 did not handle the FLV:\n{stderr}");
    let thumbnail = std::fs::read(output).unwrap();
    assert_ne!(thumbnail, include_bytes!("../../tier1/assets/placeholders/video.jpeg"));
    let image = image::load_from_memory(&thumbnail).unwrap();
    assert!(image.width() > 0 && image.height() > 0);
}
