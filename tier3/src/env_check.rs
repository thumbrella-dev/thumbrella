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
//! handler specifies its name, the command path, and which file extensions
//! it handles.  The probe walks all registered handlers and checks whether
//! the command exists and is executable.  Results are cached in the
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
//! | Executable | `which::which()` + `--version` | `ffmpeg`, `inkscape` |
//! | Registered handler | file exists + exec bit / PATH lookup | `f3d` |
//! | Runtime service | Check env var or socket | `DISPLAY` for xvfb |

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

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
    //  `sandbox::probe`) so a wedged tool never delays startup.
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

    //  USD fallback depends on both the f3d and python3/usd-core results.
    let usd_available = usd_fallback_available(&report);
    if let Some(backend) = report.backends.get_mut("usdz") {
        backend.available = usd_available;
        if !usd_available {
            backend.unavailable_reason = Some("requires f3d and python3 with usd-core".into());
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
fn subprocess_probe_jobs() -> Vec<ProbeJob> {
    vec![
        Box::new(|| job_executable("ffmpeg_cli", "image", "ffmpeg", "-version")),
        Box::new(|| job_executable("ffprobe", "video", "ffprobe", "-version")),
        Box::new(|| job_executable("magick", "image", "gm", "version")),
        Box::new(|| job_executable("oiiotool", "image", "oiiotool", "--version")),
        Box::new(office_job),
        Box::new(f3d_job),
        Box::new(blender_job),
        Box::new(playwright_job),
        Box::new(python_usd_job),
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
fn job_executable(name: &str, category: &str, binary: &str, check_arg: &str) -> ProbeResult {
    let (available, details, unavailable_reason) = try_executable(binary, check_arg);
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

//  Probe cache (TBR_PROBE)

/// External tools whose availability can be cached, in output order.
/// The token is the command name; `python3` additionally implies `usd-core`.
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
    ("python3", "python3"),
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
    ("python3", "python3", "runtime"),
];

static FORCE_PROBE: AtomicBool = AtomicBool::new(false);

/// Ignore `TBR_PROBE` and force a live probe (used by the `probe` command).
pub fn set_force_probe() {
    FORCE_PROBE.store(true, Ordering::Relaxed);
}

fn force_probe() -> bool {
    FORCE_PROBE.load(Ordering::Relaxed)
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
/// additionally enables every format the handler declares, and `python3` is
/// taken to include `usd-core` (see [`probe_envvar`]).
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
    // `python3` in the cache means the USD pipeline is ready; mark usd-core as
    // available so `usd_fallback_available` enables USDZ dispatch.
    if has("python3")
        && let Some(backend) = report.backends.get_mut("python3")
    {
        backend.details = Some(format!(
            "{}; usd-core available",
            backend.details.as_deref().unwrap_or("python3")
        ));
    }
}

/// Build the `TBR_PROBE=...` value that reproduces `report`'s tool availability.
///
/// `python3` is emitted only when `usd-core` is present, because the cache
/// treats `python3` as implying the USD pipeline.
pub fn probe_envvar(report: &EnvReport) -> String {
    let mut tokens: Vec<&str> = Vec::new();
    for (name, token) in TOOL_TOKENS {
        let Some(backend) = report.backends.get(*name) else { continue };
        let ok = if *name == "python3" {
            backend.details.as_deref().is_some_and(|d| d.contains("usd-core available"))
        } else {
            backend.available
        };
        if ok && !tokens.contains(token) {
            tokens.push(token);
        }
    }
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
        && report.backends.get("python3").is_some_and(|b| {
            b.available && b.details.as_deref().is_some_and(|d| d.contains("usd-core available"))
        })
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
                ("python3".into(), backend("python3", true, Some("usd-core available"))),
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
    let result = (|| -> Result<(String, String), String> {
        let path = which::which("blender").map_err(|e| format!("blender not found: {e}"))?;
        //  A version check is all we need: the importer surface we rely on is
        //  keyed off the release that added USD.  Running a real import pass is
        //  far too slow for startup.
        let mut cmd = std::process::Command::new(&path);
        cmd.arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = crate::sandbox::output(&mut cmd, &crate::sandbox::probe())
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
        Ok((path.display().to_string(), first_line))
    })();
    match result {
        Ok((path, version_line)) => {
            let mut result = ProbeResult::one(BackendInfo {
                name: "blender".into(),
                category: "geometry".into(),
                method,
                available: true,
                details: Some(format!("{path}: {version_line}")),
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

//  Playwright CLI probe

/// Probe for the Playwright CLI (`playwright-cli`) used for HTML snapshots.
///
/// Delegated to [`crate::html::probe`], which checks the binary and the
/// browser cache on disk.  It never installs anything and never launches a
/// browser.
fn playwright_job() -> ProbeResult {
    let (available, details, unavailable_reason) = crate::html::probe();
    ProbeResult::one(BackendInfo {
        name: crate::html::HANDLER_NAME.to_string(),
        category: "document".into(),
        method: ProbeMethod::Executable {
            binary: "playwright-cli".into(),
            check_arg: "browser cache".into(),
        },
        available,
        details,
        unavailable_reason,
    })
}

/// Return the cached environment report, or `None` if not yet probed.
pub fn cached_report() -> Option<EnvReport> {
    ENV_REPORT.read().unwrap().clone()
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
    // Registered handlers - probed by file existence + execute bit.  This is a
    // cheap in-process check that seeds an entry for every handler; the
    // subprocess probes later override the ones they can verify in detail.
    // Handlers with command "(builtin)" are always available (pure-Rust).
    let handlers = HANDLER_REGISTRY.read().unwrap().clone();
    for h in &handlers {
        let (available, details, reason, method) = if h.command == "(builtin)" {
            (true, Some(h.description.to_string()), None, ProbeMethod::Builtin)
        } else {
            let (a, d, r) = try_executable_at(h.command, h.description);
            let m = ProbeMethod::Executable {
                binary: h.command.to_string(),
                check_arg: String::new(),
            };
            (a, d, r, m)
        };
        report.backends.insert(
            h.name.to_string(),
            BackendInfo {
                name: h.name.to_string(),
                category: h.category.to_string(),
                method,
                available,
                details,
                unavailable_reason: reason,
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

//  F3D probe

/// Probe for the F3D 3D geometry renderer.
///
/// F3D `--version` crashes without a display server, so the version check is
/// only attempted when `DISPLAY` or `WAYLAND_DISPLAY` is set.  Without a
/// display we still report the binary as available if it exists and is
/// executable, but note the display requirement.
fn f3d_job() -> ProbeResult {
    let method = ProbeMethod::Executable {
        binary: "f3d".into(),
        check_arg: "--version".into(),
    };
    let path = match which::which("f3d") {
        Ok(p) => p,
        Err(_) => {
            return ProbeResult::one(BackendInfo {
                name: "f3d".into(),
                category: "geometry".into(),
                method,
                available: false,
                details: None,
                unavailable_reason: Some("f3d not found on PATH".into()),
            })
        }
    };

    let has_display =
        std::env::var("DISPLAY").ok().or_else(|| std::env::var("WAYLAND_DISPLAY").ok()).is_some();

    let (available, details, reason) = if has_display {
        let mut cmd = std::process::Command::new(&path);
        cmd.arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match crate::sandbox::output(&mut cmd, &crate::sandbox::probe()) {
            Ok(output) => {
                let first_line = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("(no output)")
                    .to_string();
                (true, Some(format!("{}: {}", path.display(), first_line)), None)
            }
            Err(e) => (
                false,
                Some(format!("found at {}", path.display())),
                Some(format!("f3d --version: {e}")),
            ),
        }
    } else {
        // F3D --version crashes without a display; just note it was found.
        (true, Some(format!("{} (display required for version check)", path.display())), None)
    };

    ProbeResult::one(BackendInfo {
        name: "f3d".into(),
        category: "geometry".into(),
        method,
        available,
        details,
        unavailable_reason: reason,
    })
}

//  Python + usd-core probe

/// Probe for Python 3 with the `usd-core` package (USDZ/USD geometry extraction).
///
/// The tier-3 USD pipeline uses a Python script to extract mesh data from
/// USDZ/USDC/USDA files, then feeds the resulting OBJ to F3D for rendering.
/// Both Python and usd-core must be present for USD support.
fn python_usd_job() -> ProbeResult {
    let method = ProbeMethod::Executable {
        binary: "python3".into(),
        check_arg: "--version".into(),
    };
    let python = match which::which("python3") {
        Ok(p) => p,
        Err(_) => {
            return ProbeResult::one(BackendInfo {
                name: "python3".into(),
                category: "runtime".into(),
                method,
                available: false,
                details: None,
                unavailable_reason: Some("python3 not found on PATH".into()),
            })
        }
    };

    // Check Python version.
    let mut cmd = std::process::Command::new(&python);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let py_ver = match crate::sandbox::output(&mut cmd, &crate::sandbox::probe()) {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            if s.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                s.to_string()
            }
        }
        Err(e) => {
            return ProbeResult::one(BackendInfo {
                name: "python3".into(),
                category: "runtime".into(),
                method,
                available: false,
                details: Some(format!("found at {}", python.display())),
                unavailable_reason: Some(format!("python3 --version: {e}")),
            })
        }
    };

    // Check for usd-core (the PyPI package that provides pxr.Usd).
    let mut cmd = std::process::Command::new(&python);
    cmd.arg("-c")
        .arg("from pxr import Usd")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let usd_ok = crate::sandbox::output(&mut cmd, &crate::sandbox::probe())
        .map(|o| o.status.success())
        .unwrap_or(false);

    ProbeResult::one(BackendInfo {
        name: "python3".into(),
        category: "runtime".into(),
        method,
        available: true,
        details: Some(format!(
            "{}: {}; usd-core {}",
            python.display(),
            py_ver,
            if usd_ok { "available" } else { "NOT FOUND" }
        )),
        unavailable_reason: if usd_ok {
            None
        } else {
            Some("usd-core not installed (pip install usd-core)".into())
        },
    })
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

/// Try to locate an executable on `$PATH` and run it with a benign check arg.
///
/// The command is bounded by [`crate::sandbox::probe`], so a tool that hangs
/// is reported unavailable rather than stalling startup.
///
/// Returns `(available, details, reason)`.
fn try_executable(binary: &str, check_arg: &str) -> (bool, Option<String>, Option<String>) {
    let path = match which::which(binary) {
        Ok(p) => p,
        Err(e) => return (false, None, Some(format!("which({binary}): {e}"))),
    };

    // Run the binary with the check arg and capture the first line of output.
    let mut cmd = std::process::Command::new(&path);
    cmd.arg(check_arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match crate::sandbox::output(&mut cmd, &crate::sandbox::probe()) {
        Ok(output) => {
            let first_line = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("(no output)")
                .to_string();
            (true, Some(format!("{}: {}", path.display(), first_line)), None)
        }
        Err(e) => (
            false,
            Some(format!("found at {}", path.display())),
            Some(format!("{binary} {check_arg}: {e}")),
        ),
    }
}

/// Check whether a file at an absolute path exists and is executable.
///
/// Does not invoke the tool - only checks metadata.  Use this for scripts
/// and binaries at known paths that do not support a `--version` flag.
///
/// Returns `(available, details, reason)`.
fn try_executable_at(path: &str, _desc: &str) -> (bool, Option<String>, Option<String>) {
    let resolved = if std::path::Path::new(path).components().count() > 1 {
        std::path::PathBuf::from(path)
    } else {
        match which::which(path) {
            Ok(p) => p,
            Err(e) => return (false, None, Some(format!("which({path}): {e}"))),
        }
    };

    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_file() => check_file_executable(meta, &resolved),
        Ok(_) => (false, None, Some(format!("{}: not a regular file", resolved.display()))),
        Err(e) => (false, None, Some(format!("{}: {e}", resolved.display()))),
    }
}

#[cfg(unix)]
fn check_file_executable(
    meta: std::fs::Metadata,
    resolved: &std::path::Path,
) -> (bool, Option<String>, Option<String>) {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    if mode & 0o111 != 0 {
        (true, Some(format!("executable at {}", resolved.display())), None)
    } else {
        (false, None, Some(format!("{}: not executable (mode {mode:o})", resolved.display())))
    }
}

#[cfg(not(unix))]
fn check_file_executable(
    _meta: std::fs::Metadata,
    resolved: &std::path::Path,
) -> (bool, Option<String>, Option<String>) {
    // Windows: just check file exists - execute bits don't apply.
    (true, Some(format!("found at {}", resolved.display())), None)
}
