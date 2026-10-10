// build.rs - placeholder icon/ETag generation + wasm32 time-type guard
//
// Regenerates icons when tier1/build_placeholders.py is edited. If Python or the
// required pip packages are absent the build continues using the committed
// JPEG files and emits a cargo warning instead of failing.
//
// Also, when building for wasm32-unknown-unknown, scans source files for
// std::time::Instant and std::time::SystemTime.  tier1 uses web_time types
// in its public API - mixing them with std::time types causes confusing
// "mismatched types" errors.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    //  wasm32 time-type guard
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("wasm32") {
        check_time_types(&manifest);
    }

    let script = Path::new(&manifest).join("build_placeholders.py");
    let out_dir = Path::new(&manifest).join("assets/placeholders");
    let generated = std::env::var("OUT_DIR").unwrap();

    println!("cargo:rerun-if-changed={}", script.display());

    // Asset changes must refresh ETags without rerunning Python and overwriting
    // those assets, or continually invalidating Cargo's watched JPEG files.
    let stamp = Path::new(&generated).join("placeholder_generator");
    let script_bytes = std::fs::read(&script).expect("read placeholder generator");
    if !stamp.exists() || std::fs::read(&stamp).expect("read generator stamp") != script_bytes {
        match Command::new("python3").arg(&script).arg("--out").arg(&out_dir).status() {
            Ok(s) if s.success() => {
                std::fs::write(&stamp, script_bytes).expect("write generator stamp");
            }
            Ok(s) => println!(
                "cargo:warning=build_placeholders.py exited with {s}; \
                 using committed placeholder files"
            ),
            Err(e) => println!(
                "cargo:warning=build_placeholders.py could not run ({e}); \
                 using committed placeholder files"
            ),
        }
    }

    let kinds = [
        "image", "video", "audio", "vector", "document", "geometry", "archive", "text", "binary", "unknown",
        "failed",
    ];
    let mut source = String::from("static PLACEHOLDER_ASSETS: [PlaceholderAsset; 11] = [\n");
    for kind in kinds {
        let path = out_dir.join(format!("{kind}.jpeg"));
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes =
            std::fs::read(&path).unwrap_or_else(|e| panic!("read placeholder {}: {e}", path.display()));
        let etag = format!("\"{:x}\"", Sha256::digest(&bytes));
        source.push_str(&format!(
            "    PlaceholderAsset {{ bytes: placeholders::{}, etag: {etag:?} }},\n",
            kind.to_ascii_uppercase(),
        ));
    }
    source.push_str("];\n");
    std::fs::write(Path::new(&generated).join("placeholder_assets.rs"), source)
        .expect("write generated placeholder metadata");
}

/// Scan tier1/src/ for std::time::Instant and std::time::SystemTime.
/// tier1 uses web_time in its public API, so these cause type mismatches
/// in downstream wasm crates.
fn check_time_types(manifest: &str) {
    let src = Path::new(manifest).join("src");
    let mut errors = Vec::new();
    scan_dir(&src, &mut errors);

    if !errors.is_empty() {
        eprintln!(
            "\n=== WASM TIME-TYPE GUARD ===\n\
             The following files in tier1/src/ use std::time::{{Instant, SystemTime}}.\n\
             tier1 uses web_time types in its public API, use `web_time` instead.\n"
        );
        for e in &errors {
            eprintln!("  {e}");
        }
        eprintln!();
        panic!(
            "wasm32 target forbids std::time::Instant and std::time::SystemTime in tier1.  See errors above."
        );
    }
}

fn scan_dir(dir: &Path, errors: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, errors);
        } else if path.extension().is_some_and(|e| e == "rs") {
            check_file(&path, errors);
        }
    }
}

fn check_file(path: &Path, errors: &mut Vec<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            continue;
        }

        if line.contains("std::time::Instant") || line.contains("std::time::SystemTime") {
            errors.push(format!(
                "{}:{}: {}",
                path.strip_prefix(std::env::current_dir().unwrap()).unwrap_or(path).display(),
                line_no + 1,
                trimmed,
            ));
        }
    }
}
