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
//! The engine stays the CLI's business.  At startup [`probe`] asks
//! `playwright-cli install-browser --list` what is actually installed, picks the
//! first engine from [`PREFERRED_BROWSERS`] that appears there, and only then
//! passes `--browser`.  When the list holds none of our preferences the flag is
//! omitted entirely and the CLI uses its own default (or its config file).
//! An empty list means there is nothing to drive, so the backend is reported
//! unavailable and HTML falls back to the normal document placeholder.
//!
//! Only the viewport is fixed, because it is part of the output contract.
//! Chromium-headless-shell entries are ignored: they are not launchable as a
//! `--browser` value on their own.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::RwLock;
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

/// Engines we are willing to name explicitly, best first.
///
/// The first three share the same rendering engine, so output stays consistent
/// between deployments; the last two are fallbacks for hosts that only have
/// them.  This list is also the set of names recognised in
/// `install-browser --list`.
const PREFERRED_BROWSERS: &[&str] = &["chromium", "chrome", "msedge", "firefox", "webkit"];

/// Engine chosen by the startup probe.
///
/// `None` before the probe runs, and also when no preferred engine was found -
/// in both cases the CLI decides for itself.
static SELECTED_BROWSER: RwLock<Option<&'static str>> = RwLock::new(None);

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
    let browser = selected_browser();

    let out_path = arena.output_path("png");
    let out_str = out_path.to_str().ok_or("non-UTF-8 scratch path")?;

    let args = open_args(url, &idle, browser, Some(config));
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

/// Startup probe: is `playwright-cli` present and able to run a snapshot?
///
/// The version check alone is not enough - the binary is useless without a
/// downloaded browser - so the probe inspects what `install-browser --list`
/// reports and then opens (and closes) a real session to prove it launches.
///
/// Returns `(available, details, unavailable_reason)`.
pub fn probe() -> (bool, Option<String>, Option<String>) {
    let path = match which::which(CLI_BINARY) {
        Ok(path) => path,
        Err(e) => return (false, None, Some(format!("{CLI_BINARY} not found: {e}"))),
    };

    let version = Command::new(&path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown version".into());
    let label = format!("{}: {version}", path.display());

    let installed = match installed_browsers() {
        Ok(installed) => installed,
        Err(error) => return (false, Some(label), Some(error)),
    };
    if installed.is_empty() {
        return (
            false,
            Some(label),
            Some("no browsers installed (playwright-cli install-browser)".into()),
        );
    }

    // Preferred engine when the host has one; otherwise let the CLI decide.
    if let Some(browser) = PREFERRED_BROWSERS.iter().copied().find(|b| installed.contains(b)) {
        match probe_launch(Some(browser)) {
            Ok(()) => {
                set_selected_browser(Some(browser));
                return (true, Some(format!("{label}, {browser}")), None);
            }
            Err(error) => debug(&format!("{browser} launch failed: {error}")),
        }
    }

    match probe_launch(None) {
        Ok(()) => {
            set_selected_browser(None);
            (true, Some(format!("{label}, default engine")), None)
        }
        Err(error) => (false, Some(label), Some(error)),
    }
}

/// Engines reported by `playwright-cli install-browser --list`.
///
/// The command prints every browser known to every Playwright installation on
/// the host as `<cache>/<engine>-<revision>` paths; only the engine name
/// matters here.  Entries for engines we do not name explicitly (headless
/// shells, ffmpeg, winldd) are dropped.
fn installed_browsers() -> Result<Vec<&'static str>, String> {
    let output = Command::new(CLI_BINARY)
        .args(["install-browser", "--list"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("list browsers: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "list browsers: {}",
            describe_failure(&output.stderr, output.status.code())
        ));
    }
    Ok(parse_installed_browsers(&String::from_utf8_lossy(&output.stdout)))
}

/// Extract engine names from the `install-browser --list` output.
fn parse_installed_browsers(listing: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for line in listing.lines() {
        let Some(entry) = line.trim().rsplit('/').next() else { continue };
        let Some((engine, revision)) = entry.rsplit_once('-') else { continue };
        if revision.is_empty() || !revision.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let engine = engine.replace('_', "-");
        if let Some(known) = PREFERRED_BROWSERS.iter().copied().find(|b| *b == engine)
            && !found.contains(&known)
        {
            found.push(known);
        }
    }
    found
}

fn set_selected_browser(browser: Option<&'static str>) {
    *SELECTED_BROWSER.write().unwrap() = browser;
}

/// Engine chosen at startup, or `None` to let the CLI decide.
fn selected_browser() -> Option<&'static str> {
    *SELECTED_BROWSER.read().unwrap()
}

/// Open and close a throwaway session to prove the browser engine works.
fn probe_launch(browser: Option<&str>) -> Result<(), String> {
    let dir = tempfile::Builder::new()
        .prefix("thumbrella-probe-")
        .tempdir()
        .map_err(|e| format!("probe workdir: {e}"))?;
    let session = format!("thumbrella-probe-{}", std::process::id());
    let _guard = SessionGuard {
        session: session.clone(),
        dir: dir.path().to_path_buf(),
    };

    let idle = idle_timeout_arg();
    let args = open_args("about:blank", &idle, browser, None);
    run_cli(dir.path(), &session, "open", &args)?;
    Ok(())
}

/// Arguments for `open`, adding `--browser` only when an engine was chosen.
fn open_args<'a>(
    url: &'a str,
    idle: &'a str,
    browser: Option<&'a str>,
    config: Option<&'a str>,
) -> Vec<&'a str> {
    let mut args = vec![url, "--idle-timeout", idle];
    if let Some(browser) = browser {
        args.push("--browser");
        args.push(browser);
    }
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
    fn parse_installed_browsers_reads_engine_names() {
        let listing = "Playwright version: 1.65.0\n  Browsers:\n    /root/.cache/ms-playwright/chromium-1252\n    /root/.cache/ms-playwright/chromium_headless_shell-1252\n    /root/.cache/ms-playwright/ffmpeg-1013\n  References:\n    /usr/lib/node_modules/playwright/node_modules/playwright-core\n";
        assert_eq!(parse_installed_browsers(listing), vec!["chromium"]);
        assert_eq!(parse_installed_browsers(""), Vec::<&str>::new());
        assert_eq!(parse_installed_browsers("  Browsers:\n    ffmpeg-1013\n"), Vec::<&str>::new());
    }

    #[test]
    fn preferred_engine_wins_over_installation_order() {
        let installed = parse_installed_browsers(
            "    /root/.cache/ms-playwright/webkit-2100\n    /root/.cache/ms-playwright/chromium-1252\n",
        );
        let chosen = PREFERRED_BROWSERS.iter().copied().find(|b| installed.contains(b));
        assert_eq!(chosen, Some("chromium"));
    }

    #[test]
    fn open_args_omit_the_browser_when_none_was_chosen() {
        assert_eq!(
            open_args("https://x/", "30000", None, None),
            vec!["https://x/", "--idle-timeout", "30000"]
        );
        assert_eq!(
            open_args("https://x/", "30000", Some("chromium"), None),
            vec!["https://x/", "--idle-timeout", "30000", "--browser", "chromium"]
        );
        assert_eq!(
            open_args("https://x/", "30000", None, Some("/tmp/c.json")),
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
