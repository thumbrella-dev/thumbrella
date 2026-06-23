//! Server diagnostics — configuration report and service validation.
//!
//! [`DiagReport`] is a structured snapshot of the server's configuration and
//! the reachability / health of every external dependency (cache backends,
//! handoff tiers, account service).  It is intended for operator use only.
//!
//! # Exposure
//!
//! The report is intentionally **not** exposed on any HTTP endpoint — it
//! contains configuration values, handoff URLs, and account identifiers that
//! must not leak to the public internet.  The only supported surface is the
//! `tier1 check` CLI subcommand.  A future opt-in via a secret token may be
//! added for remote ops tooling, but requires explicit design.
//!
//! # Usage
//!
//! ```bash
//! tier1 check          # pretty human-readable output
//! tier1 check --json   # machine-readable JSON (same struct)
//! ```

use serde::{Deserialize, Serialize};

// ── Runtime mode ──────────────────────────────────────────────────────────────

/// The execution environment this binary is running in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// `tier1` binary running as a command-line tool.
    Cli,
    /// Browser WASM module (wasm-bindgen build).
    Wasm,
    /// Cloudflare Workers isolate (workers-rs build).
    Cloudflare,
    /// Embedded as a library — no HTTP server, no runtime.
    Library,
}

// ── Tier status ───────────────────────────────────────────────────────────────

/// Availability / configuration state of a processing tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TierStatus {
    /// Renderer is built into this binary — no external dependency.
    Builtin,
    /// Tier delegates to an external handoff server.
    Handoff,
    /// No renderer is configured; this tier will be skipped.
    Missing,
    /// Configured but validation failed; see the accompanying `*_validation`
    /// field on [`DiagReport`] for the error message.
    Error,
}

// ── Validation outcome ────────────────────────────────────────────────────────

/// Result of validating a configurable external dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    /// Validation passed — dependency is reachable and responding correctly.
    Ok,
    /// Dependency is not configured; no check was performed.
    NotConfigured,
    /// Configuration is present but validation found a non-fatal issue.
    Warn,
    /// Configuration is present but validation failed.
    Error,
    /// Validation was not attempted (e.g. earlier required step also failed).
    Skipped,
}

/// Result of a single validation check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Validation {
    pub status: ValidationStatus,
    /// Human-readable description; populated on error or when config is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Validation {
    pub fn ok() -> Self {
        Self { status: ValidationStatus::Ok, message: None }
    }
    pub fn not_configured() -> Self {
        Self { status: ValidationStatus::NotConfigured, message: None }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self { status: ValidationStatus::Error, message: Some(msg.into()) }
    }
    pub fn warn(msg: impl Into<String>) -> Self {
        Self { status: ValidationStatus::Warn, message: Some(msg.into()) }
    }
    pub fn skipped() -> Self {
        Self { status: ValidationStatus::Skipped, message: None }
    }
}

// ── File-backed backend check ────────────────────────────────────────────────

/// Write-access and disk-space snapshot for a file-backed backend path.
///
/// Produced by [`check_file_path`] during [`collect`].  Never sent to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCheck {
    /// The path as written in the DSN (may be relative).
    pub path: String,
    /// Whether the process can write to this path.
    ///
    /// When the file does not yet exist the parent directory is tested instead;
    /// see `note` for which path was actually checked.
    pub writable: bool,
    /// Descriptive note when a fallback was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Free bytes available to an unprivileged user on the filesystem that
    /// hosts this path.  `None` when the query fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    /// SQLite-specific schema validation.  `None` for non-SQLite paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_validation: Option<Validation>,
}

// ── DiagReport ────────────────────────────────────────────────────────────────

/// A diagnostic section contributed by a higher tier.
///
/// Each tier can register sections at startup.  The `diag` command collects
/// and prints all sections after the main tier1 report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagSection {
    /// Section heading (e.g. `"Tier 2 — Supported Formats"`).
    pub heading: String,
    /// One entry per line item.
    pub entries: Vec<DiagEntry>,
}

/// A single line item in a diagnostic section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagEntry {
    /// Format / extension / name (e.g. `"glb"`, `"video/mp4"`).
    pub label: String,
    /// Short status string (e.g. `"available"`, `"missing"`, `"builtin"`).
    pub status: String,
    /// Optional detail (e.g. `"/opt/thumbrella/bin/3drender.sh"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Global registry of diagnostic sections contributed by higher tiers.
static DIAG_SECTIONS: std::sync::RwLock<Vec<DiagSection>> = std::sync::RwLock::new(Vec::new());

/// Register a diagnostic section.  Called at startup by tier2/tier3 before
/// `collect()` runs.
pub fn register_section(section: DiagSection) {
    DIAG_SECTIONS.write().unwrap().push(section);
}

/// Snapshot of all registered diagnostic sections.
pub fn collect_sections() -> Vec<DiagSection> {
    DIAG_SECTIONS.read().unwrap().clone()
}

/// Full server diagnostic report.
///
/// Collected by [`collect`] from environment variables and `AppConfig`.
/// Never sent over HTTP; printed by the `tier1 diag` CLI subcommand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagReport {
    // ── Identity ──────────────────────────────────────────────────────────────
    /// How this build is running.
    pub runtime: RuntimeMode,
    /// Crate version from `Cargo.toml`.
    pub version: String,
    /// Build timestamp injected by a build script, if present.
    ///
    /// Set `TBR_BUILD_TIMESTAMP` in `build.rs` via `vergen` or a similar tool
    /// to populate this field.  Absent in dev builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,

    // ── Server config ─────────────────────────────────────────────────────────
    /// HTTP port the server binds on.
    pub server_port: u16,    /// Whether the configured port is available and bindable by this process.
    pub port_available: bool,    /// Server identifier (colo code or operator label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    /// Developer / debug mode enabled.
    /// Whether local-URL access is enabled (`TBR_ALLOW_LOCAL`).
    pub allow_local: bool,
    /// Trace sink DSN if configured, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
    /// Validation result for the trace sink DSN.
    pub trace_validation: Validation,
    /// Write-access and disk-space check for the trace file (when `TBR_TRACE`
    /// uses a file-backed scheme such as `ndjson:`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_file_check: Option<FileCheck>,
    /// Whether this server requires a handshake on all endpoints (`TBR_HANDSHAKE`).
    pub handshake_set: bool,
    /// Validation result for the handshake value.
    ///
    /// Checks whether the configured `TBR_HANDSHAKE` value looks like an auth
    /// token (starts with `tbr_[a-z]_`), which would indicate a misconfiguration.
    pub handshake_validation: Validation,


    // ── Tier 1 ────────────────────────────────────────────────────────────────
    /// Tier 1 is always builtin — this field is informational only.
    pub tier1: TierStatus,
    // ── Tier 2 ────────────────────────────────────────────────────────────────
    pub tier2: TierStatus,
    /// Handoff URL or DSN, if configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2_handoff: Option<String>,
    /// Validation result for the tier 2 handoff target.
    pub tier2_validation: Validation,

    // ── Tier 3 ────────────────────────────────────────────────────────────────
    pub tier3: TierStatus,
    /// Handoff URL or DSN, if configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier3_handoff: Option<String>,
    /// Validation result for the tier 3 handoff target.
    pub tier3_validation: Validation,

    // ── Cache ─────────────────────────────────────────────────────────────────
    /// Human-readable summary of the cache configuration (backends, TTLs, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_config: Option<String>,
    /// Validation result for cache backend(s).
    pub cache_validation: Validation,
    /// Write-access and disk-space check for the cache file (when `TBR_CACHE`
    /// uses a file-backed scheme such as `sqlite:`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_file_check: Option<FileCheck>,

    // ── Overall ───────────────────────────────────────────────────────────────
    /// `true` if every required component passed validation.
    /// Optional/unconfigured components do not affect this flag.
    pub healthy: bool,

    // ── Runtime environment ───────────────────────────────────────────────────
    /// Docker image or container runtime name, if the process appears to be
    /// running inside a container.  `None` when no container heuristics match.
    ///
    /// Detection order:
    /// 1. `TBR_CONTAINER_IMAGE` env var (operator-set label)
    /// 2. `/etc/thumbrella-release` custom image descriptor
    /// 3. `/.dockerenv` present → Docker
    /// 4. `/.containerenv` present → Podman
    /// 5. `/proc/1/cgroup` contains "docker" / "containerd" / "kubepods"
    /// 6. Generic `"container (unknown image)"` when any indicator fires
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_image: Option<String>,

    // ── Extension sections (tier2 / tier3 contributions) ──────────────────────
    /// Diagnostic sections contributed by higher tiers.  Each tier registers
    /// sections at startup describing its format support and backend status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<DiagSection>,
}

// ── Collector ────────────────────────────────────────────────────────────────

/// Whether tier 2 is compiled into this binary (set at startup by tier2/tier3).
static TIER2_BUILTIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether tier 3 is compiled into this binary (set at startup by tier3).
static TIER3_BUILTIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Signal that tier 2 is built into this binary.  Call at startup from
/// `tier2` or `tier3` binaries.
pub fn mark_tier2_builtin() {
    TIER2_BUILTIN.store(true, std::sync::atomic::Ordering::Release);
}

/// Signal that tier 3 is built into this binary.  Call at startup from
/// `tier3` binaries.
pub fn mark_tier3_builtin() {
    TIER3_BUILTIN.store(true, std::sync::atomic::Ordering::Release);
}

/// True when at least one higher-tier renderer is compiled into this binary.
/// Used by the UX layer to suppress "no higher tiers configured" hints.
pub fn has_builtin_renderer() -> bool {
    TIER2_BUILTIN.load(std::sync::atomic::Ordering::Acquire)
        || TIER3_BUILTIN.load(std::sync::atomic::Ordering::Acquire)
}

/// Collect a diagnostic report for the current process environment.
///
/// All configuration is read from `AppConfig` — which itself was populated
/// from env vars at startup.  External connectivity checks (ping handoff
/// servers, connect to cache) are stubbed as `Skipped` until those subsystems
/// are wired up.
#[cfg(feature = "native")]
pub fn collect(cfg: &crate::config::AppConfig) -> DiagReport {
    // Tier 2 — prefer builtin when the renderer is compiled in, otherwise
    // check for a handoff URL, otherwise missing.
    let (tier2, tier2_handoff, tier2_validation) = if TIER2_BUILTIN.load(std::sync::atomic::Ordering::Acquire) {
        (TierStatus::Builtin, None, Validation::ok())
    } else {
        match cfg.tier2.url.as_ref() {
            Some(url) => (TierStatus::Handoff, Some(url.clone()), validate_handoff_target(url, &cfg.tier2.headers)),
            None      => (TierStatus::Missing, None,              Validation::not_configured()),
        }
    };

    // Tier 3 — same logic: builtin > handoff > missing.
    let (tier3, tier3_handoff, tier3_validation) = if TIER3_BUILTIN.load(std::sync::atomic::Ordering::Acquire) {
        (TierStatus::Builtin, None, Validation::ok())
    } else {
        match cfg.tier3.url.as_ref() {
            Some(url) => (TierStatus::Handoff, Some(url.clone()), validate_handoff_target(url, &cfg.tier3.headers)),
            None      => (TierStatus::Missing, None,              Validation::not_configured()),
        }
    };

    // Cache
    let (cache_config, cache_validation, cache_file_check) = match cfg.cache_url.as_ref() {
        Some(dsn) => {
            let mut desc = dsn.clone();
            if let Some(max) = cfg.cache_max_items {
                desc = format!("{desc}  (max_items={max})");
            }
            let (validation, file_check) = crate::cache::validate_dsn(dsn);
            (Some(desc), validation, file_check)
        }
        None => (None, Validation::not_configured(), None),
    };

    let build_timestamp = option_env!("TBR_BUILD_TIMESTAMP").map(str::to_owned);

    // Trace sink
    let trace_url = cfg.trace_url.clone();
    let (trace_validation, trace_file_check) = match cfg.trace_url.as_deref() {
        None      => (Validation::not_configured(), None),
        Some(dsn) => crate::tracelog::validate_dsn(dsn),
    };

    // Cache file check
    // (produced by cache::validate_dsn above)

    // Port availability
    let port_available = check_port_available(cfg.port);

    // Container / Docker image detection
    let container_image = detect_container_image();

    // Handshake validation — flag values that look like auth tokens.
    let handshake_validation = match cfg.handshake.as_deref() {
        None => Validation::not_configured(),
        Some(hs) if crate::config::looks_like_auth_token(hs) => {
            Validation::error(
                "looks like an auth token (starts with 'tbr_'); \
                 set a simple shared secret instead",
            )
        }
        Some(_) => Validation::ok(),
    };

    let healthy = !matches!(tier2, TierStatus::Error)
        && !matches!(tier3, TierStatus::Error)
        && !matches!(tier2_validation.status, ValidationStatus::Error)
        && !matches!(tier3_validation.status, ValidationStatus::Error)
        && !matches!(cache_validation.status, ValidationStatus::Error)
        && !matches!(trace_validation.status, ValidationStatus::Error)
        && !matches!(handshake_validation.status, ValidationStatus::Error)
        && port_available
        && cache_file_check.as_ref()
            .and_then(|fc| fc.sqlite_validation.as_ref())
            .map(|v| v.status != ValidationStatus::Error)
            .unwrap_or(true);

    DiagReport {
        runtime: RuntimeMode::Cli,
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_timestamp,
        server_port: cfg.port,
        port_available,
        server_id: cfg.server.clone(),
        allow_local: cfg.allow_local,
        trace_url,
        trace_validation,
        trace_file_check,
        handshake_set: cfg.handshake.is_some(),
        handshake_validation,
        tier1: TierStatus::Builtin,
        tier2,
        tier2_handoff,
        tier2_validation,
        tier3,
        tier3_handoff,
        tier3_validation,
        cache_config,
        cache_validation,
        cache_file_check,
        healthy,
        container_image,
        extensions: collect_sections(),
    }
}

/// Check write access and free disk space for a file-backed backend path.
///
/// Uses `access(2)` to test write permission without opening or creating
/// anything.  When the target file does not yet exist the parent directory
/// is tested instead — that is where the file will ultimately be created.
/// Free space is queried via `statvfs(2)` on the deepest existing ancestor.
///
/// Called by backend `diag()` implementations in [`crate::cache`] and
/// [`crate::tracelog`].
#[cfg(feature = "native")]
pub(crate) fn check_file_path(path: &str) -> FileCheck {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    let target = Path::new(path);

    let (check_path, note) = if target.exists() {
        (target.to_path_buf(), None)
    } else {
        let parent = target.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        (
            parent.to_path_buf(),
            Some("file does not exist; parent directory checked".to_string()),
        )
    };

    // access(2): tests with real UID/GID, never modifies the filesystem.
    let writable = CString::new(check_path.as_os_str().as_bytes())
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 })
        .unwrap_or(false);

    let free_bytes = free_bytes_at(&check_path);

    FileCheck { path: path.to_string(), writable, note, free_bytes, sqlite_validation: None }
}

/// Query free bytes on the filesystem hosting `path` via `statvfs(2)`.
///
/// Walks up to the nearest existing ancestor when the path itself does not
/// exist yet, so a configured-but-not-yet-created file path still works.
#[cfg(feature = "native")]
pub(crate) fn free_bytes_at(path: &std::path::Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut p = path.to_path_buf();
    loop {
        if p.exists() { break; }
        let parent = p.parent().map(|q| q.to_path_buf());
        match parent {
            Some(q) if !q.as_os_str().is_empty() => p = q,
            _ => p = std::path::PathBuf::from("."),
        }
        if p.exists() { break; }
    }

    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut buf) } != 0 { return None; }
    // f_bavail: blocks available to unprivileged users; f_bsize: block size.
    Some(buf.f_bavail as u64 * buf.f_bsize as u64)
}

/// Check whether the process can bind the given TCP port on `0.0.0.0`.
///
/// Uses `SO_REUSEADDR` so a brief probe bind succeeds even when the port was
/// recently held; the socket is dropped immediately after.  Returns `false`
/// when the bind fails for any reason (permission denied for ports <1024,
/// already in use, etc.).
#[cfg(feature = "native")]
fn check_port_available(port: u16) -> bool {
    use std::net::TcpListener;
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// Detect whether the current process is running inside a container and
/// attempt to identify the image name.
///
/// Returns `None` when no container indicators are found.
#[cfg(feature = "native")]
fn detect_container_image() -> Option<String> {
    // 1. Operator-supplied label via environment variable.
    if let Ok(v) = std::env::var("TBR_CONTAINER_IMAGE") {
        let v = v.trim().to_string();
        if !v.is_empty() { return Some(v); }
    }

    // 2. Thumbrella-specific release file (written into the image at build time).
    if let Ok(content) = std::fs::read_to_string("/etc/thumbrella-release") {
        let name = content.trim().to_string();
        if !name.is_empty() { return Some(name); }
    }

    // 3. Check container presence heuristics.
    let dockerenv    = std::path::Path::new("/.dockerenv").exists();
    let containerenv = std::path::Path::new("/.containerenv").exists();
    let in_cgroup    = std::fs::read_to_string("/proc/1/cgroup")
        .map(|s| {
            s.contains("docker")
            || s.contains("containerd")
            || s.contains("kubepods")
            || s.contains("lxc")
        })
        .unwrap_or(false);

    if !dockerenv && !containerenv && !in_cgroup {
        return None;
    }

    // 4. Read /etc/os-release for a descriptive name.
    //    On standard images this just gives the OS, but custom Thumbrella
    //    images can set IMAGE_ID or override PRETTY_NAME.
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        // Prefer a custom IMAGE_ID if present, then fall back to PRETTY_NAME.
        let mut pretty_name = None;
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("IMAGE_ID=") {
                let v = val.trim_matches('"').trim();
                if !v.is_empty() { return Some(v.to_string()); }
            }
            if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                pretty_name = Some(val.trim_matches('"').trim().to_string());
            }
        }
        if let Some(name) = pretty_name {
            if !name.is_empty() {
                let runtime = if dockerenv { "docker" }
                    else if containerenv { "podman" }
                    else { "container" };
                return Some(format!("{runtime} ({name})"));
            }
        }
    }

    // 5. Generic fallback: we know it's a container but not which image.
    let runtime = if dockerenv { "docker container" }
        else if containerenv { "podman container" }
        else { "container (unknown image)" };
    Some(runtime.to_string())
}

// ── Pretty printer ────────────────────────────────────────────────────────────

impl DiagReport {
    /// Print a human-readable diagnostic report to stdout.
    pub fn print_pretty(&self) {
        println!("Thumbrella — Diagnostics");
        println!("{}", "─".repeat(48));

        println!("  runtime         : {:?}", self.runtime);
        println!("  version         : {}", self.version);
        if let Some(ref ts) = self.build_timestamp {
            println!("  build_timestamp : {ts}");
        }
        if let Some(ref ci) = self.container_image {
            println!("  container_image : {ci}");
        }
        println!();

        println!("Server");
        println!("  port            : {}", self.server_port);
        let port_ok = if self.port_available { "available" } else { "UNAVAILABLE (already in use or permission denied)" };
        println!("  port_available  : {port_ok}");
        println!("  server_id       : {}", self.server_id.as_deref().unwrap_or("—"));
        println!("  allow_local     : {}", self.allow_local);
        println!("  trace_url       : {}", self.trace_url.as_deref().unwrap_or("none"));
        print_validation("  trace_validation", &self.trace_validation);
        if let Some(ref fc) = self.trace_file_check {
            print_file_check("trace_file", fc);
        }
        println!("  handshake       : {}", if self.handshake_set { "set" } else { "—" });
        print_validation("  handshake_check ", &self.handshake_validation);
        println!();

        println!("Tiers");
        print_tier("tier1", &self.tier1, None, &Validation::ok());
        print_tier("tier2", &self.tier2, self.tier2_handoff.as_deref(), &self.tier2_validation);
        print_tier("tier3", &self.tier3, self.tier3_handoff.as_deref(), &self.tier3_validation);
        println!();

        println!("Cache");
        println!("  config          : {}", self.cache_config.as_deref().unwrap_or("—"));
        print_validation("  validation", &self.cache_validation);
        if let Some(ref fc) = self.cache_file_check {
            print_file_check("file", fc);
            if let Some(ref sv) = fc.sqlite_validation {
                print_validation("    schema      ", sv);
            }
        }
        println!();

        // ── Extension sections (tier2 / tier3 contributions) ──────────────────
        for section in &self.extensions {
            println!("{}", section.heading);
            for entry in &section.entries {
                let detail = entry.detail.as_deref().unwrap_or("");
                println!("  {:<16} {:<12} {detail}", entry.label, entry.status);
            }
            println!();
        }

        let status = if self.healthy { "OK ✓" } else { "DEGRADED ✗" };
        println!("Overall: {status}");
    }
}

/// Validate an external handoff target URL and local handoff auth config.
#[cfg(feature = "native")]
fn validate_handoff_target(url: &str, headers: &std::collections::HashMap<String, String>) -> Validation {
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::time::Duration;

    if headers.is_empty() {
        return Validation::warn(
            "no auth headers configured for handoff target; \
             add x-tbr-handshake=... or a Bearer token via the connect string",
        );
    }

    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return Validation::error(format!("invalid handoff URL: {e}")),
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Validation::error("handoff URL has no host"),
    };
    let port = match parsed.port_or_known_default() {
        Some(p) => p,
        None => return Validation::error("handoff URL has no usable port"),
    };

    let addr: SocketAddr = match (host, port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => return Validation::error("handoff host resolved to no addresses"),
        },
        Err(e) => return Validation::error(format!("handoff DNS resolve failed: {e}")),
    };

    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => Validation::ok(),
        Err(e) => Validation::error(format!("handoff target unreachable ({host}:{port}): {e}")),
    }
}
fn print_tier(label: &str, status: &TierStatus, handoff: Option<&str>, validation: &Validation) {
    let status_str = match status {
        TierStatus::Builtin  => "builtin",
        TierStatus::Handoff  => "handoff",
        TierStatus::Missing  => "missing",
        TierStatus::Error    => "error",
    };
    println!("  {label:<16}: {status_str}");
    if let Some(url) = handoff {
        println!("    handoff       : {url}");
    }
    print_validation(&format!("    validation  "), validation);
}

fn print_validation(label: &str, v: &Validation) {
    let s = match v.status {
        ValidationStatus::Ok            => "ok",
        ValidationStatus::NotConfigured => "not configured",
        ValidationStatus::Warn          => "warn",
        ValidationStatus::Error         => "error",
        ValidationStatus::Skipped       => "skipped",
    };
    if let Some(ref msg) = v.message {
        println!("{label}: {s} — {msg}");
    } else {
        println!("{label}: {s}");
    }
}
fn print_file_check(label: &str, fc: &FileCheck) {
    let writable_str = if fc.writable { "yes" } else { "NO (permission denied)" };
    let note_suffix = fc.note.as_deref()
        .map(|n| format!("  ({n})"))
        .unwrap_or_default();
    println!("  {label:<16}: {}", fc.path);
    println!("    writable      : {writable_str}{note_suffix}");
    if let Some(free) = fc.free_bytes {
        println!("    free          : {}", fmt_diag_bytes(free));
    }
}

fn fmt_diag_bytes(b: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    const KIB: u64 = 1 << 10;
    if      b >= GIB { format!("{:.1} GiB", b as f64 / GIB as f64) }
    else if b >= MIB { format!("{:.1} MiB", b as f64 / MIB as f64) }
    else if b >= KIB { format!("{:.1} KiB", b as f64 / KIB as f64) }
    else             { format!("{b} B") }
}