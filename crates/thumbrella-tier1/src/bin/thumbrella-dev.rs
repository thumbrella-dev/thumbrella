//! Development CLI for quick local thumbnail experiments.
//!
//! Usage:
//!   cargo run -p thumbrella-tier1 --bin thumbrella-dev -- <input> [output-path]
//!
//! <input> may be:
//!   - an http:// or https:// URL   (fetched over HTTP)
//!   - a file:// URL                (read from local filesystem)
//!   - a plain filesystem path      (automatically promoted to file://)
//!
//! Processing follows the identical path as the HTTP server: partial prefix
//! fetch → magic sniffing → progressive/embedded/full decode → post-process.

use serde::Serialize;
use std::env;
use std::path::PathBuf;
use thumbrella_tier1::pipeline;
use thumbrella_tier1::request::{ItemRequest, RequestedOps};
use thumbrella_tier1::source::SourceRef;
use thumbrella_tier1::{ItemResult, ThumbnailProfile};

/// Thin wrapper printed to stdout; the full `ItemResult` is flattened in so
/// callers see all the same fields as the HTTP API response plus the two
/// CLI-specific fields below.
#[derive(Debug, Serialize)]
struct CliOutput {
    input_url: String,
    /// Path the thumbnail JPEG was written to (only when `thumbnail` was produced).
    output_path: String,
    profile: ThumbnailProfile,
    /// `true` when the thumbnail was written to `output_path`.
    thumbnail_written: bool,
    /// The full ItemResult — same shape as the batch API.  The `thumbnail`
    /// field is cleared here (bytes are on disk) so it won't appear in output.
    #[serde(flatten)]
    result: ItemResult,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let mut etag: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    // Accept flags in any position (before or after positional args).
    let mut raw = env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--etag" => {
                etag = Some(raw.next().ok_or_else(|| "--etag requires a value".to_string())?);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag: {other}"));
            }
            _ => positional.push(arg),
        }
    }

    let mut pos = positional.into_iter();
    let input = pos.next().ok_or_else(|| usage("missing input path or URL"))?;
    let output = pos.next();

    // Promote bare filesystem paths to file:// URLs so everything goes
    // through the same fetch code path as http/https sources.
    let url = to_url(&input)?;

    let output_path = match output {
        Some(path) => PathBuf::from(path),
        None => default_output_path(&url),
    };

    let profile = ThumbnailProfile::default();

    // Run the identical pipeline as the HTTP server: partial prefix fetch,
    // magic-byte sniffing, progressive / embedded / full decode, post-process.
    let item = ItemRequest {
        id: None,
        source: SourceRef::Url { url: url.clone() },
        etag,
        ops: RequestedOps::default(),
    };
    let mut result = pipeline::process_item(&item, &profile).await;

    // Write thumbnail to disk and clear the bytes so they don't pollute stdout.
    let thumbnail_written = if let Some(ref jpeg) = result.thumbnail {
        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!("failed to create output directory {}: {e}", parent.display())
                })?;
            }
        }
        std::fs::write(&output_path, jpeg)
            .map_err(|e| format!("failed to write output {}: {e}", output_path.display()))?;
        result.thumbnail = None;
        true
    } else {
        false
    };

    let cli_out = CliOutput {
        input_url: url,
        output_path: output_path.display().to_string(),
        profile,
        thumbnail_written,
        result,
    };

    let json = serde_json::to_string_pretty(&cli_out)
        .map_err(|e| format!("failed to serialize output JSON: {e}"))?;
    println!("{json}");

    Ok(())
}

/// Convert the user-supplied input string to a URL.
/// - http:// / https:// → used as-is
/// - file:// → used as-is
/// - anything else → treated as a filesystem path and promoted to file://
fn to_url(input: &str) -> Result<String, String> {
    if input.starts_with("http://")
        || input.starts_with("https://")
        || input.starts_with("file://")
    {
        return Ok(input.to_string());
    }

    // Bare path: resolve to absolute and encode as file://.
    let path = PathBuf::from(input);
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot determine working directory: {e}"))?
            .join(path)
    };

    // Encode path components that need it (spaces → %20, etc.).
    let encoded = abs
        .to_str()
        .ok_or_else(|| format!("path contains non-UTF-8 characters: {}", abs.display()))?
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            _ => c.to_string(),
        })
        .collect::<String>();

    Ok(format!("file://{encoded}"))
}

fn default_output_path(url: &str) -> PathBuf {
    // Extract the filename portion of the URL as the stem.
    let stem = url
        .rsplit('/')
        .next()
        .and_then(|s| {
            // Strip query string if present.
            let base = s.split('?').next().unwrap_or(s);
            // Strip extension.
            base.rsplit('.').nth(1).or(Some(base))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or("thumbrella");

    PathBuf::from(format!("{stem}.thumb.jpg"))
}

fn usage(msg: &str) -> String {
    format!(
        "{msg}\nusage: cargo run -p thumbrella-tier1 --bin thumbrella-dev -- [--etag <etag>] <input> [output-path]\n  <input> may be a file path, file:// URL, http:// URL, or https:// URL\n  --etag   pass a previously received etag to test the 304/not_modified path"
    )
}

