//! HTML web page snapshots via `playwright-cli`.
//!
//! This renderer is deliberately different from the other tier-3 backends:
//! it never stages the source bytes.  Tier 1 has already proved the URL
//! returns a successful HTML response, and the browser loads the page itself
//! (so relative CSS, fonts, and images resolve exactly as they would in a
//! browser).  The original URL is handed to `playwright-cli`, the viewport is
//! sized, and the screenshot is read back from the scratch arena.
//!
//! The same path serves MHTML web archives: they are self-contained pages, so
//! the browser renders them without any unpacking on our side.  Local inputs
//! (`file://`) are loaded through `run-code` because `open` blocks that
//! protocol.
//!
//! # Environment contract
//!
//! Thumbrella does not install or configure Playwright.  It expects a working
//! `playwright-cli` on `$PATH` with a browser already downloaded:
//!
//! ```text
//! npm install -g @playwright/cli
//! playwright-cli install-browser chromium --no-shell
//! ```
//!
//! The engine stays the CLI's business: Thumbrella never installs or configures
//! Playwright and never passes `--browser`, so the CLI uses whatever engine it
//! is set up to use.  At startup [`probe`] only checks that the binary exists
//! and that the browser cache holds a launchable engine, by reading the cache
//! directory (`PLAYWRIGHT_BROWSERS_PATH`, else Playwright's default location)
//! instead of invoking Node.  With no engine present the backend is reported
//! unavailable and HTML falls back to the normal document placeholder.
//!
//! Only the viewport is fixed, because it is part of the output contract.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use image::DynamicImage;
use tier1::RenderCook;
use tier1::renderer::RenderOutput;

use crate::scratch::ScratchArena;

/// Name under which the handler is registered in the environment check.
pub const HANDLER_NAME: &str = "playwright_cli";

/// Canonical extensions routed to this module.
///
/// `mhtml` is a self-contained web archive: the browser loads it through the
/// same `file://` or `http(s)://` URL path as a live page, so it needs no
/// special handling beyond being recognised as a document.
pub const HTML_EXTENSIONS: &[&str] = &["html", "xhtml", "mhtml"];

/// Command invoked for a snapshot.
const CLI_BINARY: &str = "playwright-cli";

/// Bytes reserved in the scratch arena for a screenshot.
const MAX_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;

/// Browser viewport used for snapshots.  5:4 matches the Thumbrella output
/// canvas, so the screenshot fills the canvas without letterboxing, and
/// 1000x800 is an exact 4x reduction to the 250x200 default thumbnail -
/// integer decimation stays sharper than an arbitrary ratio.
const VIEWPORT: (u32, u32) = (1000, 800);

/// Browser switches applied to every snapshot session.
///
/// These do not change the rendered pixels - a render with and without them
/// is byte-identical - and they do not measurably change timing, because
/// rasterization is only a few dozen milliseconds of the render.  They are
/// here to shrink what the runtime depends on: no GPU stack, no shared-memory
/// backing, no first-run or background-update work, no text subpixel/LCD
/// rendering paths.
const LAUNCH_ARGS: &[&str] = &[
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "--disable-accelerated-2d-canvas",
    "--disable-gpu-compositing",
    "--font-render-hinting=none",
    "--disable-lcd-text",
    "--hide-scrollbars",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-sync",
    "--disable-extensions",
];

/// Monotonic suffix so concurrent renders never share a browser session.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// True when `ext` is rendered by this module.
pub fn is_html_extension(ext: &str) -> bool {
    HTML_EXTENSIONS.contains(&ext)
}

/// True when the backend passed the startup probe.
pub fn is_available() -> bool {
    crate::env_check::cached_report()
        .and_then(|r| r.backends.get(HANDLER_NAME).map(|b| b.available))
        .unwrap_or(false)
}

//  Render

/// Render an HTML page to an image.
///
/// Returns `false` when the format is not ours or Playwright is unavailable,
/// letting the pipeline fall through to the placeholder / handoff path.
pub async fn render(cook: &mut dyn RenderCook, ext: &str) -> bool {
    if !is_html_extension(ext) {
        return false;
    }
    if !is_available() {
        cook.note(&format!("HTML snapshot requires {HANDLER_NAME} (playwright-cli)"));
        return false;
    }

    // The body is never consumed: Chromium fetches the page itself so that
    // relative resources and scripts run against the real origin.
    let url = cook.input_url().to_string();
    let result = tokio::task::spawn_blocking(move || snapshot(&url)).await;

    match result {
        Ok(Ok(image)) => {
            // No source properties: the screenshot dimensions describe our
            // own canvas, not the page, so reporting them as width/height
            // would be misleading metadata.
            tier1::renderer::apply_render_output(
                cook,
                RenderOutput {
                    image,
                    renderer: Some("playwright-cli".into()),
                    codec: None,
                    video_seek_secs: None,
                    properties: None,
                },
            );
            true
        }
        Ok(Err(error)) => {
            debug(&error);
            cook.fail_cook(&format!("playwright-cli: {error}"));
            true
        }
        Err(_) => {
            cook.fail_cook("playwright-cli: snapshot task panicked");
            true
        }
    }
}

/// Load `url` in a headless browser and return the viewport screenshot.
///
/// A unique browser session is created per call and closed on the way out,
/// including on the error paths.
fn snapshot(url: &str) -> Result<DynamicImage, String> {
    let arena = ScratchArena::new(MAX_SNAPSHOT_BYTES).map_err(|e| format!("scratch arena: {e}"))?;
    let work_dir = arena.root();

    let session = format!(
        "thumbrella-{}-{}",
        std::process::id(),
        SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    // Declared after the arena so the browser closes before the directory
    // disappears under it.
    let _guard = SessionGuard {
        session: session.clone(),
        dir: work_dir.to_path_buf(),
    };

    // The viewport is applied at launch, not after the page has loaded.  That
    // removes a CLI invocation, and more importantly it means the first layout
    // and first paint already happen at the final size: scripts that read the
    // window size on load see 1000x800 instead of the CLI default, and the page
    // never reflows underneath a screenshot.
    //
    // Local pages (and MHTML archives, which are self-contained pages) arrive
    // as `file:` URLs, which the CLI blocks unless the session opts in.  The
    // allowance is scoped to those renders so remote pages keep the stricter
    // default.
    let local = url.starts_with("file://");
    let launch = launch_config(local);
    let viewport_config = arena
        .stage_bytes(launch.as_bytes(), "cli.config.json")
        .map_err(|e| format!("stage launch config: {e}"))?;
    let config = viewport_config.to_str().ok_or("non-UTF-8 config path")?;

    // `--idle-timeout` reuses the render deadline: the daemon is spawned
    // detached, so it can outlive the process group we kill on timeout.
    let idle = idle_timeout_arg();

    let out_path = arena.output_path("png");
    let out_str = out_path.to_str().ok_or("non-UTF-8 scratch path")?;

    let args = open_args(url, &idle, Some(config));
    run_cli(work_dir, &session, "open", &args)?;
    run_cli(work_dir, &session, "screenshot", &["--filename", out_str, "--type", "png"])?;

    let bytes = arena.read_output(&out_path).map_err(|e| format!("read screenshot: {e}"))?;
    let image = image::load_from_memory(&bytes).map_err(|e| format!("decode screenshot: {e}"))?;
    Ok(image)
}

/// Browser session guard - closes the session even when a step fails.
struct SessionGuard {
    session: String,
    dir: std::path::PathBuf,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut cmd = cli_command();
        cmd.arg(format!("-s={}", self.session))
            .arg("close")
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Best effort: a wedged daemon must not hold up the render.
        let _ = crate::sandbox::output(&mut cmd, &crate::sandbox::browser_close());
    }
}

/// Run one `playwright-cli` command against `session` and return its JSON.
fn run_cli(
    dir: &Path,
    session: &str,
    command: &str,
    args: &[&str],
) -> Result<serde_json::Value, String> {
    let mut cmd = cli_command();
    cmd.arg(format!("-s={session}"))
        .arg("--json")
        .arg(command)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = crate::sandbox::output(&mut cmd, &crate::sandbox::browser())
        .map_err(|e| format!("{command}: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Option<serde_json::Value> = serde_json::from_str(stdout.trim()).ok();
    let is_error =
        parsed.as_ref().and_then(|v| v.get("isError")).and_then(|v| v.as_bool()).unwrap_or(false);

    if output.status.success() && !is_error {
        return Ok(parsed.unwrap_or(serde_json::Value::Null));
    }

    // Prefer the CLI's own error string, then the last meaningful stderr line.
    if let Some(message) = parsed.as_ref().and_then(|v| v.get("error")).and_then(|v| v.as_str()) {
        return Err(format!("{command}: {message}"));
    }
    Err(format!("{command}: {}", describe_failure(&output.stderr, output.status.code())))
}

fn describe_failure(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("Error:"))
        .or_else(|| text.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("exited without output");
    match code {
        Some(code) => format!("exit {code}: {line}"),
        None => line.to_string(),
    }
}

//  Probe

/// Startup probe: is `playwright-cli` present with a browser engine installed?
///
/// The probe is deliberately cheap and read-only: it checks that the binary
/// exists, then lists the browser cache on disk for a launchable engine.  It
/// never installs anything, never launches a browser, and never passes
/// `--browser`, so the CLI uses whatever engine it is configured to use.
///
/// Returns `(available, details, unavailable_reason)`.
pub fn probe() -> (bool, Option<String>, Option<String>) {
    let path = match which::which(CLI_BINARY) {
        Ok(path) => path,
        Err(e) => return (false, None, Some(format!("{CLI_BINARY} not found: {e}"))),
    };

    let dir = browsers_dir();
    let engines = installed_engines(&dir);
    let label = format!("{}: {}", path.display(), dir.display());
    if engines.is_empty() {
        return (
            false,
            Some(label),
            Some(format!(
                "no browser engine installed in {} (install one with playwright-cli)",
                dir.display()
            )),
        );
    }

    (true, Some(format!("{label} ({})", engines.join(", "))), None)
}

/// Directory Playwright stores downloaded browsers in.
///
/// `PLAYWRIGHT_BROWSERS_PATH` wins when set (it is in the tier3 image); the
/// rest mirror Playwright's own defaults for Linux hosts.
fn browsers_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH") {
        return PathBuf::from(path);
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(cache).join("ms-playwright");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache").join("ms-playwright");
    }
    PathBuf::from(".cache/ms-playwright")
}

/// Recognise a browser-cache entry (`chromium-1252`) as a launchable engine.
///
/// Headless-shell, ffmpeg and winldd entries are not launchable on their own
/// and are ignored.
fn engine_from_entry(name: &str) -> Option<&'static str> {
    let (engine, revision) = name.rsplit_once('-')?;
    if revision.is_empty() || !revision.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match engine {
        "chromium" => Some("chromium"),
        "firefox" => Some("firefox"),
        "webkit" => Some("webkit"),
        _ => None,
    }
}

/// Launchable engines present in `dir`, in a stable preference order.
fn installed_engines(dir: &Path) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(engine) = engine_from_entry(name)
                && !found.contains(&engine)
            {
                found.push(engine);
            }
        }
    }
    ["chromium", "firefox", "webkit"].into_iter().filter(|e| found.contains(e)).collect()
}

/// Arguments for `open`.  The engine is left to the CLI's own default.
fn open_args<'a>(url: &'a str, idle: &'a str, config: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec![url, "--idle-timeout", idle];
    if let Some(config) = config {
        args.push("--config");
        args.push(config);
    }
    args
}

/// Launch configuration written for every snapshot session.
///
/// Carries the fixed viewport and the browser switches in [`LAUNCH_ARGS`].
/// Deliberately no `browserName`: the engine stays whatever the CLI (or the
/// operator's own config) decides, so this file cannot override a host's
/// browser choice.
///
/// `allow_local_files` lifts the CLI's `file:` navigation guard, which local
/// pages and MHTML archives need.  It is only enabled for those renders.
fn launch_config(allow_local_files: bool) -> String {
    let mut config = serde_json::json!({
        "browser": {
            "contextOptions": {
                "viewport": { "width": VIEWPORT.0, "height": VIEWPORT.1 }
            },
            "launchOptions": { "args": LAUNCH_ARGS }
        }
    });
    if allow_local_files {
        config["allowUnrestrictedFileAccess"] = serde_json::Value::Bool(true);
    }
    config.to_string()
}

//  Command construction

fn cli_command() -> Command {
    Command::new(CLI_BINARY)
}

/// Session idle timeout, in milliseconds.
///
/// Derived from the render deadline rather than invented separately: a session
/// must not outlive the render that created it by more than one deadline.
fn idle_timeout_arg() -> String {
    crate::sandbox::browser().timeout.as_millis().to_string()
}

fn debug(message: &str) {
    if tier1::ux::show_raw_logs() {
        eprintln!("[tier3] playwright-cli: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_config_allows_local_files_only_when_asked() {
        let remote: serde_json::Value = serde_json::from_str(&launch_config(false)).unwrap();
        assert!(remote.get("allowUnrestrictedFileAccess").is_none(), "remote pages keep the guard");

        let local: serde_json::Value = serde_json::from_str(&launch_config(true)).unwrap();
        assert_eq!(local["allowUnrestrictedFileAccess"], serde_json::Value::Bool(true));
    }

    #[test]
    fn html_extensions_are_recognised() {
        assert!(is_html_extension("html"));
        assert!(is_html_extension("xhtml"));
        assert!(!is_html_extension("htm"));
        assert!(!is_html_extension("pdf"));
    }

    #[test]
    fn failure_description_prefers_the_error_line() {
        let stderr =
            b"node:internal/foo\nError: Browser \"chrome-for-testing\" is not installed\n    at x\n";
        assert_eq!(
            describe_failure(stderr, Some(1)),
            "exit 1: Error: Browser \"chrome-for-testing\" is not installed"
        );
        assert_eq!(describe_failure(b"", None), "exited without output");
    }

    #[test]
    fn viewport_is_an_exact_fourth_of_the_output_canvas() {
        // The default thumbnail is 250x200 (5:4).  The viewport must stay a
        // whole multiple of it so the deliver step decimates by an integer.
        const OUTPUT: (u32, u32) = (250, 200);
        assert_eq!(VIEWPORT.0 % OUTPUT.0, 0);
        assert_eq!(VIEWPORT.1 % OUTPUT.1, 0);
        assert_eq!(VIEWPORT.0 / OUTPUT.0, VIEWPORT.1 / OUTPUT.1);
        assert_eq!(VIEWPORT.0 / OUTPUT.0, 4);
    }

    #[test]
    fn recognises_launchable_engines_from_cache_entries() {
        assert_eq!(engine_from_entry("chromium-1252"), Some("chromium"));
        assert_eq!(engine_from_entry("firefox-1234"), Some("firefox"));
        assert_eq!(engine_from_entry("webkit-2203"), Some("webkit"));
        assert_eq!(engine_from_entry("chromium_headless_shell-1252"), None);
        assert_eq!(engine_from_entry("ffmpeg-1013"), None);
        assert_eq!(engine_from_entry("chromium"), None);
        assert_eq!(engine_from_entry("chromium-"), None);
    }

    #[test]
    fn open_args_adds_only_the_config_when_present() {
        assert_eq!(
            open_args("https://x/", "30000", None),
            vec!["https://x/", "--idle-timeout", "30000"]
        );
        assert_eq!(
            open_args("https://x/", "30000", Some("/tmp/c.json")),
            vec!["https://x/", "--idle-timeout", "30000", "--config", "/tmp/c.json"]
        );
    }

    #[test]
    fn launch_config_carries_the_viewport_and_switches() {
        let config: serde_json::Value = serde_json::from_str(&launch_config(false)).unwrap();
        assert_eq!(config["browser"]["contextOptions"]["viewport"]["width"], VIEWPORT.0);
        assert_eq!(config["browser"]["contextOptions"]["viewport"]["height"], VIEWPORT.1);
        assert_eq!(config["browser"]["launchOptions"]["args"], serde_json::json!(LAUNCH_ARGS));
        assert!(config["browser"].get("browserName").is_none(), "engine must stay the CLI's call");
    }
}
