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

    //  Builtin backends (always available)
    probe_builtins(&mut report);

    //  Shared library backends
    probe_shared_libraries(&mut report);

    //  Executable backends
    probe_executables(&mut report);

    //  F3D (geometry renderer with X11 dependency)
    probe_f3d(&mut report);
    probe_blender(&mut report);

    //  Playwright CLI (HTML page snapshots)
    probe_playwright(&mut report);

    //  Python + usd-core (USDZ geometry pipeline)
    probe_python_usd(&mut report);
    let usd_available = usd_fallback_available(&report);
    if let Some(backend) = report.backends.get_mut("usdz") {
        backend.available = usd_available;
        if !usd_available {
            backend.unavailable_reason = Some("requires f3d and python3 with usd-core".into());
        }
    }

    //  Runtime service backends
    probe_runtime_services(&mut report);

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
    fn parses_only_marked_valid_probe_output() {
        assert_eq!(
            parse_blender_formats(b"Blender\nTHUMBRELLA_FORMATS=[\"fbx\",\"stl\"]\n").unwrap(),
            BTreeSet::from(["fbx".into(), "stl".into()])
        );
        assert!(parse_blender_formats(b"Blender").is_err());
        assert!(parse_blender_formats(b"THUMBRELLA_FORMATS=not-json").is_err());
        assert_eq!(
            parse_blender_addons(b"THUMBRELLA_ADDONS=[\"cycles\",\"pose_library\"]\n").unwrap(),
            vec!["cycles".to_string(), "pose_library".to_string()]
        );
        assert!(parse_blender_addons(b"Blender").is_err());
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

fn parse_blender_formats(stdout: &[u8]) -> Result<BTreeSet<String>, String> {
    let text = std::str::from_utf8(stdout).map_err(|e| format!("invalid probe output: {e}"))?;
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("THUMBRELLA_FORMATS="))
        .ok_or("missing Blender format probe output")?;
    serde_json::from_str(json).map_err(|e| format!("invalid Blender format probe output: {e}"))
}

fn parse_blender_addons(stdout: &[u8]) -> Result<Vec<String>, String> {
    let text = std::str::from_utf8(stdout).map_err(|e| format!("invalid probe output: {e}"))?;
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix("THUMBRELLA_ADDONS="))
        .ok_or("missing Blender addon probe output")?;
    serde_json::from_str(json).map_err(|e| format!("invalid Blender addon probe output: {e}"))
}

fn probe_blender(report: &mut EnvReport) {
    let method = ProbeMethod::Executable {
        binary: "blender".into(),
        check_arg: "format probe".into(),
    };
    let result = (|| -> Result<(String, BTreeSet<String>, Vec<String>), String> {
        let path = which::which("blender").map_err(|e| format!("blender not found: {e}"))?;
        let arena = crate::scratch::ScratchArena::new(1024 * 1024)
            .map_err(|e| format!("probe scratch: {e}"))?;
        let script = arena
            .stage_bytes(include_str!("blender_render.py").as_bytes(), "blender_render.py")
            .map_err(|e| format!("stage probe: {e}"))?;
        // Shared builder keeps the probe's security flags and environment
        // containment identical to the render path.  The probe does not need
        // an X display, so it runs Blender directly instead of via xvfb-run.
        let mut cmd = crate::blender::probe_command(&script, &arena)
            .map_err(|e| format!("prepare Blender probe: {e}"))?;
        cmd.args(["--", "probe"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = crate::sandbox::output(&mut cmd, &crate::sandbox::blender())
            .map_err(|e| format!("Blender probe: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "Blender probe exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok((
            path.display().to_string(),
            parse_blender_formats(&output.stdout)?,
            parse_blender_addons(&output.stdout)?,
        ))
    })();
    let (available, details, unavailable_reason) = match result {
        Ok((path, formats, addons)) => {
            // Addon names are diagnostic noise for `check`/`formats`; keep them
            // out of the backend details and only emit them in raw log mode.
            if matches!(std::env::var("TBR_LOG").as_deref(), Ok("full")) {
                eprintln!("[tier3] blender addons: {}", addons.join(", "));
            }
            let detail = format!("{path}: {}", formats.iter().cloned().collect::<Vec<_>>().join(", "));
            report.blender_formats = formats;
            (true, Some(detail), None)
        }
        Err(error) => (false, None, Some(error)),
    };
    report.backends.insert(
        "blender".into(),
        BackendInfo {
            name: "blender".into(),
            category: "geometry".into(),
            method,
            available,
            details,
            unavailable_reason,
        },
    );
}

//  Playwright CLI probe

/// Probe for the Playwright CLI (`playwright-cli`) used for HTML snapshots.
///
/// Finding the binary is not enough - the browser engine has to be installed
/// too - so the probe is delegated to [`crate::html::probe`], which opens and
/// closes a real headless session.
fn probe_playwright(report: &mut EnvReport) {
    let (available, details, unavailable_reason) = crate::html::probe();
    report.backends.insert(
        crate::html::HANDLER_NAME.to_string(),
        BackendInfo {
            name: crate::html::HANDLER_NAME.to_string(),
            category: "document".into(),
            method: ProbeMethod::Executable {
                binary: "playwright-cli".into(),
                check_arg: "open about:blank".into(),
            },
            available,
            details,
            unavailable_reason,
        },
    );
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

fn probe_executables(report: &mut EnvReport) {
    // Each entry is (backend_name, category, binary, check_arg, description).
    //
    // These are external command-line tools invoked as subprocesses.
    // Tier 3 runs them with input on stdin and captures stdout.
    let candidates: &[(&str, &str, &str, &str, &str)] = &[
        ("ffmpeg_cli", "image", "ffmpeg", "-version", "FFmpeg CLI (JPEG fallback / transcoding)"),
        ("ffprobe", "video", "ffprobe", "-version", "FFmpeg media probe"),
        ("magick", "image", "gm", "version", "GraphicsMagick (arithmetic JPEG, resize)"),
        (
            "oiiotool",
            "image",
            "oiiotool",
            "--version",
            "OpenImageIO (EXR, HDR, DPX, studio formats)",
        ),
    ];

    for (name, category, binary, check_arg, desc) in candidates {
        let (available, details, reason) = try_executable(binary, check_arg, desc);
        report.backends.insert(
            name.to_string(),
            BackendInfo {
                name: name.to_string(),
                category: category.to_string(),
                method: ProbeMethod::Executable {
                    binary: binary.to_string(),
                    check_arg: check_arg.to_string(),
                },
                available,
                details,
                unavailable_reason: reason,
            },
        );
    }

    // Registered handlers - probed by file existence + execute bit.
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
fn probe_f3d(report: &mut EnvReport) {
    let path = match which::which("f3d") {
        Ok(p) => p,
        Err(_) => {
            report.backends.insert(
                "f3d".into(),
                BackendInfo {
                    name: "f3d".into(),
                    category: "geometry".into(),
                    method: ProbeMethod::Executable {
                        binary: "f3d".into(),
                        check_arg: "--version".into(),
                    },
                    available: false,
                    details: None,
                    unavailable_reason: Some("f3d not found on PATH".into()),
                },
            );
            return;
        }
    };

    let has_display =
        std::env::var("DISPLAY").ok().or_else(|| std::env::var("WAYLAND_DISPLAY").ok()).is_some();

    let (details, reason) = if has_display {
        match std::process::Command::new(&path)
            .arg("--version")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(output) => {
                let first_line = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("(no output)")
                    .to_string();
                (Some(format!("{}: {}", path.display(), first_line)), None)
            }
            Err(e) => {
                (Some(format!("found at {}", path.display())), Some(format!("f3d --version: {e}")))
            }
        }
    } else {
        // F3D --version crashes without a display; just note it was found.
        (Some(format!("{} (display required for version check)", path.display())), None)
    };

    report.backends.insert(
        "f3d".into(),
        BackendInfo {
            name: "f3d".into(),
            category: "geometry".into(),
            method: ProbeMethod::Executable {
                binary: "f3d".into(),
                check_arg: "--version".into(),
            },
            available: true,
            details,
            unavailable_reason: reason,
        },
    );
}

//  Python + usd-core probe

/// Probe for Python 3 with the `usd-core` package (USDZ/USD geometry extraction).
///
/// The tier-3 USD pipeline uses a Python script to extract mesh data from
/// USDZ/USDC/USDA files, then feeds the resulting OBJ to F3D for rendering.
/// Both Python and usd-core must be present for USD support.
fn probe_python_usd(report: &mut EnvReport) {
    let python = match which::which("python3") {
        Ok(p) => p,
        Err(_) => {
            report.backends.insert(
                "python3".into(),
                BackendInfo {
                    name: "python3".into(),
                    category: "runtime".into(),
                    method: ProbeMethod::Executable {
                        binary: "python3".into(),
                        check_arg: "--version".into(),
                    },
                    available: false,
                    details: None,
                    unavailable_reason: Some("python3 not found on PATH".into()),
                },
            );
            return;
        }
    };

    // Check Python version.
    let py_ver = std::process::Command::new(&python)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            if s.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| "unknown version".into());

    // Check for usd-core (the PyPI package that provides pxr.Usd).
    let usd_ok = std::process::Command::new(&python)
        .arg("-c")
        .arg("from pxr import Usd")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    report.backends.insert(
        "python3".into(),
        BackendInfo {
            name: "python3".into(),
            category: "runtime".into(),
            method: ProbeMethod::Executable {
                binary: "python3".into(),
                check_arg: "--version".into(),
            },
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

/// Try to locate an executable on `$PATH` and run it with a benign check arg.
///
/// Returns `(available, details, reason)`.
fn try_executable(
    binary: &str,
    check_arg: &str,
    _desc: &str,
) -> (bool, Option<String>, Option<String>) {
    let path = match which::which(binary) {
        Ok(p) => p,
        Err(e) => return (false, None, Some(format!("which({binary}): {e}"))),
    };

    // Run the binary with the check arg and capture the first line of output.
    match std::process::Command::new(&path)
        .arg(check_arg)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
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
