//! Environment capability probe.
//!
//! Tier 3 renderers depend on the host environment — shared libraries that
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
//! [`EnvReport`] and consumed by the renderer dispatch and `tier3 diag`.
//!
//! # Design
//!
//! Probes are **lazy and non-blocking** — each one runs once via
//! [`probe_environment`] and caches its result.
//!
//! # Probe types
//!
//! | Type | Method | Example |
//! |------|--------|---------|
//! | Shared library | `libloading::Library::new()` | `libpdfium.so` |
//! | Executable | `which::which()` + `--version` | `ffmpeg`, `inkscape` |
//! | Registered handler | file exists + exec bit | `/opt/thumbrella/bin/3drender.sh` |
//! | Runtime service | Check env var or socket | `DISPLAY` for xvfb |

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::RwLock;

// ── Handler registry ──────────────────────────────────────────────────────────

/// A registered subprocess handler that tier 3 may invoke for specific
/// file extensions.
#[derive(Debug, Clone)]
pub struct HandlerDecl {
    /// Unique name (e.g. `"3drender"`, `"usdrender"`).
    pub name: &'static str,
    /// Broad category for diag grouping (e.g. `"geometry"`).
    pub category: &'static str,
    /// Absolute path to the command.
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

// ── Backend descriptor ────────────────────────────────────────────────────────

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

// ── Environment report ────────────────────────────────────────────────────────

/// Full environment capability report produced at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvReport {
    /// Backend status for every known renderer, keyed by backend name.
    pub backends: BTreeMap<String, BackendInfo>,
    /// Human-readable summary of the environment.
    pub summary: String,
}

/// Global cache of environment probe results.
static ENV_REPORT: RwLock<Option<EnvReport>> = RwLock::new(None);

// ── Probe runner ──────────────────────────────────────────────────────────────

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
        summary: String::new(),
    };

    // ── Builtin backends (always available) ───────────────────────────────────
    probe_builtins(&mut report);

    // ── Shared library backends ──────────────────────────────────────────────
    probe_shared_libraries(&mut report);

    // ── Executable backends ──────────────────────────────────────────────────
    probe_executables(&mut report);

    // ── Runtime service backends ─────────────────────────────────────────────
    probe_runtime_services(&mut report);

    // ── Build summary ─────────────────────────────────────────────────────────
    let available: Vec<&str> = report.backends.values()
        .filter(|b| b.available)
        .map(|b| b.name.as_str())
        .collect();
    let unavailable: Vec<&str> = report.backends.values()
        .filter(|b| !b.available)
        .map(|b| b.name.as_str())
        .collect();

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

/// Return the cached environment report, or `None` if not yet probed.
pub fn cached_report() -> Option<EnvReport> {
    ENV_REPORT.read().unwrap().clone()
}

// ── Individual probe helpers ──────────────────────────────────────────────────

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
        report.backends.insert(name.to_string(), BackendInfo {
            name: name.to_string(),
            category: category.to_string(),
            method: ProbeMethod::Builtin,
            available: true,
            details: Some(desc.to_string()),
            unavailable_reason: None,
        });
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
        report.backends.insert(name.to_string(), BackendInfo {
            name: name.to_string(),
            category: category.to_string(),
            method: ProbeMethod::SharedLibrary { soname: soname.to_string() },
            available,
            details,
            unavailable_reason: reason,
        });
    }
}

fn probe_executables(report: &mut EnvReport) {
    // Each entry is (backend_name, category, binary, check_arg, description).
    //
    // These are external command-line tools invoked as subprocesses.
    // Tier 3 runs them with input on stdin and captures stdout.
    let candidates: &[(&str, &str, &str, &str, &str)] = &[
        ("ffmpeg_cli", "image", "ffmpeg", "-version", "FFmpeg CLI (JPEG fallback / transcoding)"),
        ("magick",    "image", "gm", "version", "GraphicsMagick (arithmetic JPEG, resize)"),
        ("oiiotool",  "image", "oiiotool", "--version", "OpenImageIO (EXR, HDR, DPX, studio formats)"),
        ("bwrap",     "runtime", "bwrap", "--version", "Bubblewrap sandbox (subprocess isolation)"),
    ];

    for (name, category, binary, check_arg, desc) in candidates {
        let (available, details, reason) = try_executable(binary, check_arg, desc);
        report.backends.insert(name.to_string(), BackendInfo {
            name: name.to_string(),
            category: category.to_string(),
            method: ProbeMethod::Executable {
                binary: binary.to_string(),
                check_arg: check_arg.to_string(),
            },
            available,
            details,
            unavailable_reason: reason,
        });
    }

    // Registered handlers — probed by file existence + execute bit.
    let handlers = HANDLER_REGISTRY.read().unwrap().clone();
    for h in &handlers {
        let (available, details, reason) = try_executable_at(h.command, h.description);
        report.backends.insert(h.name.to_string(), BackendInfo {
            name: h.name.to_string(),
            category: h.category.to_string(),
            method: ProbeMethod::Executable {
                binary: h.command.to_string(),
                check_arg: String::new(),
            },
            available,
            details,
            unavailable_reason: reason,
        });
    }
}

fn probe_runtime_services(report: &mut EnvReport) {
    // Check for a display server (xvfb / X11 / Wayland).
    let has_display = std::env::var("DISPLAY").ok()
        .or_else(|| std::env::var("WAYLAND_DISPLAY").ok());
    let (available, details, reason) = match has_display {
        Some(d) => (true, Some(format!("display server: {d}")), None),
        None => (false, None, Some("no DISPLAY or WAYLAND_DISPLAY set".to_string())),
    };
    report.backends.insert("display_server".to_string(), BackendInfo {
        name: "display_server".to_string(),
        category: "runtime".to_string(),
        method: ProbeMethod::RuntimeService {
            description: "X11/Wayland display server for headful renderers".to_string(),
        },
        available,
        details,
        unavailable_reason: reason,
    });
}

// ── Low-level probe helpers ───────────────────────────────────────────────────

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
        Err(e) => {
            (false, None, Some(format!("dlopen({soname}): {e}")))
        }
    }
}

/// Try to locate an executable on `$PATH` and run it with a benign check arg.
///
/// Returns `(available, details, reason)`.
fn try_executable(binary: &str, check_arg: &str, _desc: &str) -> (bool, Option<String>, Option<String>) {
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
        Err(e) => {
            (false, Some(format!("found at {}", path.display())),
             Some(format!("{binary} {check_arg}: {e}")))
        }
    }
}

/// Check whether a file at an absolute path exists and is executable.
///
/// Does not invoke the tool — only checks metadata.  Use this for scripts
/// and binaries at known paths that do not support a `--version` flag.
///
/// Returns `(available, details, reason)`.
fn try_executable_at(path: &str, _desc: &str) -> (bool, Option<String>, Option<String>) {
    let p = std::path::Path::new(path);
    match std::fs::metadata(p) {
        Ok(meta) if meta.is_file() => {
            // Check if any execute bit is set (owner, group, or other).
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            if mode & 0o111 != 0 {
                (true, Some(format!("executable at {path}")), None)
            } else {
                (false, None, Some(format!("{path}: not executable (mode {mode:o})")))
            }
        }
        Ok(_) => (false, None, Some(format!("{path}: not a regular file"))),
        Err(e) => (false, None, Some(format!("{path}: {e}"))),
    }
}
