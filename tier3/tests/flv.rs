use std::process::Command;

fn flv_format(path: &std::ffi::OsStr) -> serde_json::Value {
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
        .find(|entry| entry["extension"] == "flv")
        .unwrap()
        .clone()
}

#[cfg(unix)]
#[test]
fn flv_is_unavailable_without_ffprobe() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let ffmpeg = dir.path().join("ffmpeg");
    std::fs::write(&ffmpeg, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
    let entry = flv_format(dir.path().as_os_str());
    assert_eq!(entry["tier"], 3);
    assert_eq!(entry["renderer"], "ffmpeg_cli");
    assert_eq!(entry["available"], false);
}

#[test]
#[ignore = "requires ffmpeg and ffprobe on PATH"]
fn flv_uses_tier3_ffmpeg_to_render_a_frame() {
    let path = std::env::var_os("PATH").unwrap();
    let entry = flv_format(&path);
    assert_eq!(entry["available"], true);
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
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("[ffmpeg] exit="),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let thumbnail = std::fs::read(output).unwrap();
    assert_ne!(thumbnail, include_bytes!("../../tier1/assets/placeholders/video.jpeg"));
    let image = image::load_from_memory(&thumbnail).unwrap();
    assert!(image.width() > 0 && image.height() > 0);
}
