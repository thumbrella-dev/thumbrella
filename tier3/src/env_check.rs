//! Environment capability probe.
//!
//! Tier 3 renderers depend on the host environment - shared libraries that
//! may or may not be installed, command-line tools at known paths, and
//! runtime services like `xvfb`.  This module probes the environment at
//! startup and produces an [`EnvReport`] describing which backends are
//! available.
//!
//! # Handler registry
//!
//! Tier 3 backends are declared via [`register_handler`] at startup.  Each
//! handler specifies its name, the command, and which file extensions it
//! handles.  External tools are considered available when their command can be
//! invoked with a lightweight `--version`-style argument (the OS resolves the
//! name; there is no separate path/existence check).  Results are cached in the
//! [`EnvReport`] and consumed by the renderer dispatch and `tier3 check`.
//!
//! # Design
//!
//! Probes are **lazy and non-blocking** - each one runs once via
//! [`probe_environment`] and caches its result.
//!
//! # Probe types
//!
//! | Type | Method | Example |
//! |------|--------|---------|
//! | Shared library | `libloading::Library::new()` | `libpdfium.so` |
//! | Executable | run `<tool> --version` (spawn success) | `ffmpeg`, `f3d` |
//! | Runtime service | Check env var or socket | `DISPLAY` for xvfb |

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

//  Handler registry

/// A registered subprocess handler that tier 3 may invoke for specific
/// file extensions.
#[derive(Debug, Clone)]
pub struct HandlerDecl {
    /// Unique name (e.g. `"3drender"`, `"usdrender"`).
    pub name: &'static str,
    /// Broad category for check grouping (e.g. `"geometry"`).
    pub category: &'static str,
    /// Executable name or absolute path to the command.
    pub command: &'static str,
    /// File extensions this handler claims (e.g. `&["glb", "gltf"]`).
    pub extensions: &'static [&'static str],
    /// Human-readable description.
    pub description: &'static str,
}

/// Global registry of all tier-3 subprocess handlers.  Populated at startup
/// before `probe_environment()` is called.
static HANDLER_REGISTRY: RwLock<Vec<HandlerDecl>> = RwLock::new(Vec::new());

/// Register a subprocess handler.  Call before `probe_environment()`.
pub fn register_handler(h: HandlerDecl) {
    HANDLER_REGISTRY.write().unwrap().push(h);
}

/// Snapshot of the handler registry.  Returns all registered handlers
/// regardless of whether they passed the probe.
pub fn registered_handlers() -> Vec<HandlerDecl> {
    HANDLER_REGISTRY.read().unwrap().clone()
}

//  Backend descriptor

/// A single renderer backend that tier 3 may use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    /// Human-readable label (e.g. `"pdfium"`, `"inkscape"`, `"blender"`).
    pub name: String,
    /// What this backend renders (e.g. `"document"`, `"vector"`, `"geometry"`).
    pub category: String,
    /// Probe method used to detect this backend.
    pub method: ProbeMethod,
    /// Whether the backend was detected and is available.
    pub available: bool,
    /// Details about the detected installation (version string, path, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Human-readable reason why the backend is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

/// How a backend was (or would be) detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeMethod {
    /// Linked directly into the binary (always available).
    Builtin,
    /// Detected via `dlopen` / `libloading::Library::new()`.
    SharedLibrary { soname: String },
    /// Detected via `which` on `$PATH` + a benign invocation.
    Executable { binary: String, check_arg: String },
    /// Detected via environment variable or socket check.
    RuntimeService { description: String },
}

//  Environment report

/// Full environment capability report produced at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvReport {
    /// Backend status for every known renderer, keyed by backend name.
    pub backends: BTreeMap<String, BackendInfo>,
    /// Extensions whose Blender import operators passed the startup probe.
    #[serde(default)]
    pub blender_formats: BTreeSet<String>,
    /// Human-readable summary of the environment.
    pub summary: String,
}

/// Global cache of environment probe results.
static ENV_REPORT: RwLock<Option<EnvReport>> = RwLock::new(None);

//  Probe runner

/// Run all environment probes and return a report.
///
/// This is called once at startup.  Results are cached globally for subsequent
/// `diag` queries.
pub fn probe_environment() -> EnvReport {
    // Check if we already have a cached report.
    if let Some(report) = ENV_REPORT.read().unwrap().as_ref() {
        return report.clone();
    }

    let mut report = EnvReport {
        backends: BTreeMap::new(),
        blender_formats: BTreeSet::new(),
        summary: String::new(),
    };

    //  In-process probes: cheap, no subprocesses.
    probe_builtins(&mut report);
    probe_shared_libraries(&mut report);
    probe_handler_metadata(&mut report);
    probe_runtime_services(&mut report);

    //  When a probe cache is supplied (and we are not re-probing), trust it
    //  instead of spawning any external command.  Otherwise run the subprocess
    //  probes concurrently, each bounded by a hard deadline (see
    //  `probe_config`) so a wedged tool never delays startup.
    if let Some(tokens) = configured_probe_tokens().filter(|_| !force_probe()) {
        apply_probe_cache(&mut report, &tokens);
    } else {
        for result in run_probes_parallel(subprocess_probe_jobs()) {
            if let Some(formats) = result.blender_formats {
                report.blender_formats = formats;
            }
            for backend in result.backends {
                report.backends.insert(backend.name.clone(), backend);
            }
        }
    }

    //  Every registered external handler must end up in the report.  A missing
    //  entry means someone forgot to add a probe job; surface it as unavailable
    //  rather than letting dispatch silently lose the format.
    ensure_handlers_reported(&mut report);

    //  USD fallback depends on both the f3d and python3/usd-core results.
    let usd_available = usd_fallback_available(&report);
    if let Some(backend) = report.backends.get_mut("usdz") {
        backend.available = usd_available;
        if !usd_available {
            backend.unavailable_reason =
                Some("requires f3d and python3 with importable usd-core (py3-usd)".into());
        }
    }

    //  Build summary
    let available: Vec<&str> =
        report.backends.values().filter(|b| b.available).map(|b| b.name.as_str()).collect();
    let unavailable: Vec<&str> =
        report.backends.values().filter(|b| !b.available).map(|b| b.name.as_str()).collect();

    report.summary = format!(
        "{} backends available ({}), {} unavailable ({})",
        available.len(),
        available.join(", "),
        unavailable.len(),
        unavailable.join(", "),
    );

    // Cache globally.
    *ENV_REPORT.write().unwrap() = Some(report.clone());

    report
}

//  Parallel probe runner

/// The outcome of one subprocess probe.
struct ProbeResult {
    backends: Vec<BackendInfo>,
    /// Populated only by the Blender probe when it succeeds.
    blender_formats: Option<BTreeSet<String>>,
}

impl ProbeResult {
    fn one(backend: BackendInfo) -> Self {
        Self { backends: vec![backend], blender_formats: None }
    }
}

/// A probe that spawns external commands and therefore runs on its own thread.
type ProbeJob = Box<dyn FnOnce() -> ProbeResult + Send>;

/// Every probe that shells out to an external tool.
///
/// Each job runs a lightweight version/help switch and requires the exit status
/// it declares (0 unless a tool's switch is known to differ).  A tool that
/// starts but exits with an unexpected status is treated as unavailable.
///
/// Every registered external handler needs a job here (or a `CACHED_TOOLS`
/// entry for cached mode); `ensure_handlers_reported` surfaces any that are
/// missing instead of silently leaving the backend absent.
fn subprocess_probe_jobs() -> Vec<ProbeJob> {
    vec![
        Box::new(|| job_executable("ffmpeg_cli", "image", "ffmpeg", "-version", 0)),
        Box::new(|| job_executable("ffprobe", "video", "ffprobe", "-version", 0)),
        Box::new(|| job_executable("magick", "image", "gm", "version", 0)),
        Box::new(|| job_executable("oiiotool", "image", "oiiotool", "--version", 0)),
        Box::new(|| job_executable("pdftoppm", "document", "pdftoppm", "-v", 0)),
        Box::new(|| job_executable("f3d", "geometry", "f3d", "--version", 0)),
        Box::new(|| {
            job_executable("playwright_cli", "document", "playwright-cli", "--version", 0)
        }),
        Box::new(|| job_executable("xvfb_run", "runtime", "xvfb-run", "--help", 0)),
        Box::new(python_usd_job),
        Box::new(office_job),
        Box::new(blender_job),
    ]
}

/// Run every probe job on its own thread and collect the results.
///
/// A job that panics is dropped rather than aborting startup; its backend is
/// then simply absent from the report (treated as unavailable).
fn run_probes_parallel(jobs: Vec<ProbeJob>) -> Vec<ProbeResult> {
    let handles: Vec<_> = jobs.into_iter().map(std::thread::spawn).collect();
    handles.into_iter().filter_map(|handle| handle.join().ok()).collect()
}

/// Build a `ProbeResult` for a tool checked by running `<binary> <check_arg>`.
///
/// `expected_exit` is the exit status that means "the tool ran normally"; most
/// version switches return 0, but it can be set per command for tools that
/// differ.
fn job_executable(
    name: &str,
    category: &str,
    binary: &str,
    check_arg: &str,
    expected_exit: i32,
) -> ProbeResult {
    let (available, details, unavailable_reason) = try_executable(binary, check_arg, expected_exit);
    ProbeResult::one(BackendInfo {
        name: name.to_string(),
        category: category.to_string(),
        method: ProbeMethod::Executable {
            binary: binary.to_string(),
            check_arg: check_arg.to_string(),
        },
        available,
        details,
        unavailable_reason,
    })
}

/// Probe for Python 3 with an importable `usd-core` (the USD pipeline's Python
/// dependency).
///
/// A present `python3` is not enough: the `pxr` bindings have to import and
/// initialise.  The probe imports the module and creates an in-memory stage,
/// which catches the common ways a wheel install breaks (wrong Python version,
/// missing native libraries, ABI mismatch, partial install).
fn python_usd_job() -> ProbeResult {
    let method = ProbeMethod::Executable {
        binary: "python3".into(),
        check_arg: "-c import usd".into(),
    };

    let version = run_capture("python3", &["--version"], 0)
        .map(|o| output_first_line(&o))
        .unwrap_or_else(|_| "unknown version".to_string());

    let import = run_capture(
        "python3",
        &["-c", "from pxr import Usd; Usd.Stage.CreateInMemory()"],
        0,
    );
    let (available, unavailable_reason) = match import {
        Ok(_) => (true, None),
        Err(error) => (
            false,
            Some(format!(
                "usd-core import failed ({error}); install with pip install usd-core"
            )),
        ),
    };

    ProbeResult::one(BackendInfo {
        name: "python3".into(),
        category: "runtime".into(),
        method,
        available,
        details: Some(format!(
            "python3 {version}; usd-core {}",
            if available { "importable" } else { "unavailable" }
        )),
        unavailable_reason,
    })
}

//  Probe cache (TBR_PROBE)

/// External tools whose availability can be cached, in output order.
///
/// The token normally matches the command name.  Python is the exception: the
/// `python3` backend only means "python3 with an importable usd-core", so it is
/// emitted as `py3-usd` to leave room for other Python-library probes later.
const TOOL_TOKENS: &[(&str, &str)] = &[
    ("oiiotool", "oiiotool"),
    ("pdftoppm", "pdftoppm"),
    ("playwright_cli", "playwright-cli"),
    ("magick", "gm"),
    ("blender", "blender"),
    ("f3d", "f3d"),
    ("soffice", "soffice"),
    ("ffmpeg_cli", "ffmpeg"),
    ("ffprobe", "ffprobe"),
    ("python3", "py3-usd"),
    ("xvfb_run", "xvfb-run"),
];

/// Backend metadata used when replaying a probe cache:
/// `(backend name, token/command, category)`.
const CACHED_TOOLS: &[(&str, &str, &str)] = &[
    ("oiiotool", "oiiotool", "image"),
    ("pdftoppm", "pdftoppm", "document"),
    ("playwright_cli", "playwright-cli", "document"),
    ("magick", "gm", "image"),
    ("blender", "blender", "geometry"),
    ("f3d", "f3d", "geometry"),
    ("soffice", "soffice", "document"),
    ("ffmpeg_cli", "ffmpeg", "image"),
    ("ffprobe", "ffprobe", "video"),
    ("python3", "py3-usd", "runtime"),
    ("xvfb_run", "xvfb-run", "runtime"),
];

static FORCE_PROBE: AtomicBool = AtomicBool::new(false);

/// Ignore `TBR_PROBE` and force a live probe (used by the `probe` command).
pub fn set_force_probe() {
    FORCE_PROBE.store(true, Ordering::Relaxed);
}

fn force_probe() -> bool {
    FORCE_PROBE.load(Ordering::Relaxed)
}

pub(crate) fn probe_config() -> crate::sandbox::SandboxConfig {
    let mut config = crate::sandbox::probe();
    if force_probe() {
        config.timeout = Duration::from_secs(10);
    }
    config
}

/// Parse `TBR_PROBE` as a comma-separated list of available tools.
fn configured_probe_tokens() -> Option<Vec<String>> {
    let raw = std::env::var("TBR_PROBE").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(
        raw.split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Populate the external-tool backends from a `TBR_PROBE` token list.
///
/// Listed tools are available; everything else is unavailable.  `blender`
/// additionally enables every format the handler declares; the `py3-usd` token
/// marks python3 with an importable `usd-core` (see [`probe_envvar`]).
fn apply_probe_cache(report: &mut EnvReport, tokens: &[String]) {
    let has = |token: &str| tokens.iter().any(|t| t == token);
    for (name, token, category) in CACHED_TOOLS {
        let available = has(token);
        report.backends.insert(
            (*name).to_string(),
            BackendInfo {
                name: (*name).to_string(),
                category: (*category).to_string(),
                method: ProbeMethod::Executable {
                    binary: (*token).to_string(),
                    check_arg: String::new(),
                },
                available,
                details: available.then(|| format!("TBR_PROBE: {token}")),
                unavailable_reason: if available {
                    None
                } else {
                    Some("not listed in TBR_PROBE".to_string())
                },
            },
        );
    }

    if has("blender") {
        report.blender_formats = blender_handler_extensions();
    }
}

/// Build the `TBR_PROBE=...` value that reproduces `report`'s tool availability.
pub fn probe_envvar(report: &EnvReport) -> String {
    let mut tokens: Vec<&str> = Vec::new();
    for (name, token) in TOOL_TOKENS {
        let Some(backend) = report.backends.get(*name) else { continue };
        let ok = backend.available;
        if ok && !tokens.contains(token) {
            tokens.push(token);
        }
    }
    tokens.sort_unstable();
    format!("TBR_PROBE={}", tokens.join(","))
}

/// Available handlers for an extension in registration order.
pub fn available_handlers(report: &EnvReport, ext: &str) -> Vec<HandlerDecl> {
    select_available_handlers(report, ext, registered_handlers())
}

fn select_available_handlers(
    report: &EnvReport,
    ext: &str,
    handlers: Vec<HandlerDecl>,
) -> Vec<HandlerDecl> {
    handlers
        .into_iter()
        .filter(|h| h.extensions.contains(&ext))
        .filter(|h| {
            let available = |name: &str| report.backends.get(name).is_some_and(|b| b.available);
            match h.name {
                "blender" => available("blender") && report.blender_formats.contains(ext),
                "usdz" => usd_fallback_available(report),
                "ffmpeg_cli" => available("ffmpeg_cli") && available("ffprobe"),
                _ => available(h.name),
            }
        })
        .collect()
}

fn usd_fallback_available(report: &EnvReport) -> bool {
    report.backends.get("f3d").is_some_and(|b| b.available)
        && report.backends.get("python3").is_some_and(|b| b.available)
}

#[cfg(test)]
mod blender_tests {
    use super::*;

    fn handler(name: &'static str, extensions: &'static [&'static str]) -> HandlerDecl {
        HandlerDecl {
            name,
            category: "geometry",
            command: name,
            extensions,
            description: "",
        }
    }

    fn backend(name: &str, available: bool, details: Option<&str>) -> BackendInfo {
        BackendInfo {
            name: name.into(),
            category: "geometry".into(),
            method: ProbeMethod::Builtin,
            available,
            details: details.map(str::to_string),
            unavailable_reason: None,
        }
    }

    #[test]
    fn parses_blender_version() {
        assert_eq!(parse_blender_version("Blender 4.5.15\n"), Some((4, 5)));
        assert_eq!(parse_blender_version("Blender 3.0.0\nBuild: abc"), Some((3, 0)));
        assert_eq!(parse_blender_version("Blender 4\n"), Some((4, 0)));
        assert_eq!(parse_blender_version("not blender"), None);
        assert_eq!(parse_blender_version(""), None);
    }

    #[test]
    fn selects_capable_handlers_in_priority_order() {
        let handlers = vec![
            handler("blender", &["fbx", "usd"]),
            handler("f3d", &["fbx"]),
            handler("usdz", &["usd"]),
        ];
        let mut report = EnvReport {
            backends: BTreeMap::from([
                ("blender".into(), backend("blender", true, None)),
                ("f3d".into(), backend("f3d", true, None)),
                ("python3".into(), backend("python3", true, None)),
            ]),
            blender_formats: BTreeSet::from(["fbx".into()]),
            summary: String::new(),
        };
        let names = |ext| {
            select_available_handlers(&report, ext, handlers.clone())
                .into_iter()
                .map(|h| h.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("fbx"), vec!["blender", "f3d"]);
        assert_eq!(names("usd"), vec!["usdz"]);
        report.backends.get_mut("f3d").unwrap().available = false;
        assert!(select_available_handlers(&report, "usd", handlers).is_empty());
    }
}

/// Oldest Blender that ships the USD importer reached through the `usd*`
/// handler extensions.  Alembic (abc), glTF/GLB, DAE and PLY importers all
/// predate it by years, so one version threshold covers every extension the
/// Blender handler declares.
const MIN_BLENDER_VERSION: (u32, u32) = (3, 0);

/// Parse `(major, minor)` from `blender --version` output such as
/// `Blender 4.5.15`.
fn parse_blender_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.split_once("Blender")?.1.trim_start();
    let token = rest.split_whitespace().next()?;
    let mut parts = token.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Extensions declared by the registered `blender` handler.
///
/// Populated into the format-availability set when the installed Blender is
/// new enough to have every importer we depend on.
fn blender_handler_extensions() -> BTreeSet<String> {
    registered_handlers()
        .into_iter()
        .find(|h| h.name == "blender")
        .map(|h| h.extensions.iter().map(|e| e.to_string()).collect())
        .unwrap_or_default()
}

fn blender_job() -> ProbeResult {
    let method = ProbeMethod::Executable {
        binary: "blender".into(),
        check_arg: "--version".into(),
    };
    let result = (|| -> Result<String, String> {
        //  A version check is all we need: the importer surface we rely on is
        //  keyed off the release that added USD.  Running a real import pass is
        //  far too slow for startup.
        let mut cmd = std::process::Command::new("blender");
        cmd.arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = crate::sandbox::output(&mut cmd, &probe_config())
            .map_err(|e| format!("blender --version: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "blender --version exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let first_line = text.lines().next().unwrap_or("").trim().to_string();
        let version = parse_blender_version(&text)
            .ok_or_else(|| format!("could not parse Blender version from '{first_line}'"))?;
        if version < MIN_BLENDER_VERSION {
            return Err(format!(
                "Blender {}.{} is too old (need {}.{}+ for USD import)",
                version.0, version.1, MIN_BLENDER_VERSION.0, MIN_BLENDER_VERSION.1,
            ));
        }
        Ok(first_line)
    })();
    match result {
        Ok(version_line) => {
            let mut result = ProbeResult::one(BackendInfo {
                name: "blender".into(),
                category: "geometry".into(),
                method,
                available: true,
                details: Some(format!("blender --version: {version_line}")),
                unavailable_reason: None,
            });
            //  A new-enough Blender has every importer the handler declares, so
            //  gate dispatch on the version rather than a per-format probe.
            result.blender_formats = Some(blender_handler_extensions());
            result
        }
        Err(error) => ProbeResult::one(BackendInfo {
            name: "blender".into(),
            category: "geometry".into(),
            method,
            available: false,
            details: None,
            unavailable_reason: Some(error),
        }),
    }
}

/// Return the cached environment report, or `None` if not yet probed.
pub fn cached_report() -> Option<EnvReport> {
    ENV_REPORT.read().unwrap().clone()
}

/// True when `xvfb-run` passed its probe.
///
/// Renderers use this to decide whether to wrap a GUI tool in a virtual display
/// when no real one is present.
pub fn xvfb_available() -> bool {
    cached_report().and_then(|r| r.backends.get("xvfb_run").map(|b| b.available)).unwrap_or(false)
}

//  Individual probe helpers

fn probe_builtins(report: &mut EnvReport) {
    // These are compiled into tier3 and always available.
    let builtins = [
        ("ffmpeg", "video", "libav-based decode (FFmpeg static)"),
        ("image_crate", "image", "Pure-Rust image crate decode"),
        ("resvg", "vector", "Pure-Rust SVG renderer (resvg)"),
        ("jxl_oxide", "image", "Pure-Rust JPEG XL decoder"),
        ("raw_preview", "image", "TIFF/RAW embedded preview extractor"),
    ];

    for (name, category, desc) in builtins {
        report.backends.insert(
            name.to_string(),
            BackendInfo {
                name: name.to_string(),
                category: category.to_string(),
                method: ProbeMethod::Builtin,
                available: true,
                details: Some(desc.to_string()),
                unavailable_reason: None,
            },
        );
    }
}

fn probe_shared_libraries(report: &mut EnvReport) {
    // Each entry is (backend_name, category, soname, description).
    //
    // These are optional shared libraries that tier3 can use if present.
    // They are probed via dlopen at startup; only available ones are
    // registered in the dispatch table.
    let candidates: &[(&str, &str, &str, &str)] = &[
        // ("pdfium", "document", "libpdfium.so", "PDF rendering (first page)"),
        // ("libvips", "image", "libvips.so.42", "libvips high-performance image processing"),
    ];

    for (name, category, soname, desc) in candidates {
        let (available, details, reason) = try_dlopen(soname, desc);
        report.backends.insert(
            name.to_string(),
            BackendInfo {
                name: name.to_string(),
                category: category.to_string(),
                method: ProbeMethod::SharedLibrary {
                    soname: soname.to_string(),
                },
                available,
                details,
                unavailable_reason: reason,
            },
        );
    }
}

fn probe_handler_metadata(report: &mut EnvReport) {
    //  Pure-Rust handlers (command "(builtin)") have no executable to probe, so
    //  seed them here.  External handlers are registered by their own version
    //  probes (see `subprocess_probe_jobs`); `ensure_handlers_reported` catches
    //  any that are missing.
    for h in HANDLER_REGISTRY.read().unwrap().clone() {
        if h.command != "(builtin)" {
            continue;
        }
        report.backends.insert(
            h.name.to_string(),
            BackendInfo {
                name: h.name.to_string(),
                category: h.category.to_string(),
                method: ProbeMethod::Builtin,
                available: true,
                details: Some(h.description.to_string()),
                unavailable_reason: None,
            },
        );
    }
}

/// Ensure every registered external handler has a report entry.
///
/// A missing entry almost always means a probe job was forgotten.  Mark it
/// unavailable with a clear reason so `check`/`formats` show the omission
/// instead of the format silently disappearing from dispatch.
fn ensure_handlers_reported(report: &mut EnvReport) {
    for h in HANDLER_REGISTRY.read().unwrap().clone() {
        if report.backends.contains_key(h.name) {
            continue;
        }
        report.backends.insert(
            h.name.to_string(),
            BackendInfo {
                name: h.name.to_string(),
                category: h.category.to_string(),
                method: ProbeMethod::Executable {
                    binary: h.command.to_string(),
                    check_arg: String::new(),
                },
                available: false,
                details: None,
                unavailable_reason: Some(format!("no probe registered for {}", h.command)),
            },
        );
    }
}

//  Office (soffice) probe

/// Validate a `soffice` build (LibreOffice identity and version).
///
/// Delegated to [`crate::office::probe`], which only runs `soffice --version`
/// and overrides the file-existence entry seeded by `probe_handler_metadata`.
fn office_job() -> ProbeResult {
    let (available, details, unavailable_reason) = crate::office::probe();
    ProbeResult::one(BackendInfo {
        name: crate::office::HANDLER_NAME.to_string(),
        category: "document".to_string(),
        method: ProbeMethod::Executable {
            binary: "soffice".into(),
            check_arg: "--version".into(),
        },
        available,
        details,
        unavailable_reason,
    })
}

fn probe_runtime_services(report: &mut EnvReport) {
    // Check for a display server (xvfb / X11 / Wayland).
    let has_display = std::env::var("DISPLAY").ok().or_else(|| std::env::var("WAYLAND_DISPLAY").ok());
    let (available, details, reason) = match has_display {
        Some(d) => (true, Some(format!("display server: {d}")), None),
        None => (false, None, Some("no DISPLAY or WAYLAND_DISPLAY set".to_string())),
    };
    report.backends.insert(
        "display_server".to_string(),
        BackendInfo {
            name: "display_server".to_string(),
            category: "runtime".to_string(),
            method: ProbeMethod::RuntimeService {
                description: "X11/Wayland display server for headful renderers".to_string(),
            },
            available,
            details,
            unavailable_reason: reason,
        },
    );
}

//  Low-level probe helpers

/// Try to open a shared library via dlopen.
///
/// Returns `(available, details, reason)`.
fn try_dlopen(soname: &str, _desc: &str) -> (bool, Option<String>, Option<String>) {
    match unsafe { libloading::Library::new(soname) } {
        Ok(lib) => {
            // Library opened successfully.
            drop(lib);
            (true, Some(format!("dlopen({soname}) succeeded")), None)
        }
        Err(e) => (false, None, Some(format!("dlopen({soname}): {e}"))),
    }
}

/// Run a tool and require a specific exit status.
///
/// The binary is executed directly; the OS resolves it on `$PATH`, so there is
/// no separate "is it on the path?" step.  `expected_exit` lets each command
/// define what "ran normally" means (most version switches return 0).
///
/// A tool that hangs is reported unavailable rather than stalling startup.
fn run_capture(
    binary: &str,
    args: &[&str],
    expected_exit: i32,
) -> Result<std::process::Output, String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output =
        crate::sandbox::output(&mut cmd, &probe_config()).map_err(|e| format!("run {binary}: {e}"))?;
    match output.status.code() {
        Some(code) if code == expected_exit => Ok(output),
        Some(code) => {
            let detail = output_last_error_line(&output);
            let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
            Err(format!("exit {code} (expected {expected_exit}){detail}"))
        }
        None => Err("terminated by signal".to_string()),
    }
}

/// First non-empty line of stdout, falling back to stderr.
fn output_first_line(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
        return line.to_string();
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
        .to_string()
}

/// Last non-empty line of stderr (the useful line for tracebacks), falling
/// back to stdout.
fn output_last_error_line(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(line) = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).next_back() {
        return line.to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
        .unwrap_or("")
        .to_string()
}

/// Run `<binary> <check_arg>` and report availability from a successful exit.
///
/// Returns `(available, details, reason)`.
fn try_executable(
    binary: &str,
    check_arg: &str,
    expected_exit: i32,
) -> (bool, Option<String>, Option<String>) {
    match run_capture(binary, &[check_arg], expected_exit) {
        Ok(output) => {
            let first_line = output_first_line(&output);
            (true, Some(format!("{binary} {check_arg}: {first_line}")), None)
        }
        Err(e) => (false, None, Some(format!("{binary} {check_arg}: {e}"))),
    }
}
