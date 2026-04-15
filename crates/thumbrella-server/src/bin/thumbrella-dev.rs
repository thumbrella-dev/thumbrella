//! Development CLI for quick local thumbnail experiments.
//!
//! Usage:
//!   cargo run -p thumbrella-server --bin thumbrella-dev -- <input-path> [output-path]

use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use thumbrella_server::pipeline;
use thumbrella_types::{SourceMetadata, ThumbnailProfile};

#[derive(Debug, Serialize)]
struct CliOutput {
    input_path: String,
    output_path: String,
    profile: ThumbnailProfile,
    source_meta: SourceMetadata,
    render: Option<pipeline::RenderInfo>,
    error: Option<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let input = args
        .next()
        .ok_or_else(|| usage("missing input file path"))?;
    let output = args.next();

    let input_path = PathBuf::from(input);
    if !input_path.exists() {
        return Err(format!("input path does not exist: {}", input_path.display()));
    }

    let bytes = fs::read(&input_path)
        .map_err(|e| format!("failed to read input file {}: {e}", input_path.display()))?;

    let fs_meta = fs::metadata(&input_path)
        .map_err(|e| format!("failed to stat input file {}: {e}", input_path.display()))?;

    let modified = fs_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| format!("{}", d.as_secs()));

    let source_meta = pipeline::metadata_from_local_bytes(&bytes, Some(fs_meta.len()), modified);
    let profile = ThumbnailProfile::default();

    let output_path = match output {
        Some(path) => PathBuf::from(path),
        None => default_output_path(&input_path),
    };

    let mut cli_out = CliOutput {
        input_path: input_path.display().to_string(),
        output_path: output_path.display().to_string(),
        profile: profile.clone(),
        source_meta,
        render: None,
        error: None,
    };

    match pipeline::render_thumbnail_from_bytes(&bytes, &profile) {
        Ok((jpeg, info)) => {
            if let Some(parent) = output_path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("failed to create output directory {}: {e}", parent.display()))?;
                }
            }

            fs::write(&output_path, jpeg)
                .map_err(|e| format!("failed to write output {}: {e}", output_path.display()))?;

            cli_out.render = Some(info);
        }
        Err(err) => {
            cli_out.error = Some(err);
        }
    }

    let json = serde_json::to_string_pretty(&cli_out)
        .map_err(|e| format!("failed to serialize output JSON: {e}"))?;
    println!("{json}");

    Ok(())
}

fn default_output_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("thumbrella");

    PathBuf::from(format!("{stem}.thumb.jpg"))
}

fn usage(msg: &str) -> String {
    format!(
        "{msg}\nusage: cargo run -p thumbrella-server --bin thumbrella-dev -- <input-path> [output-path]"
    )
}
