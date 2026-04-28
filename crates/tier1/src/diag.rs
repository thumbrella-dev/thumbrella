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
//! `tier1 diag` CLI subcommand.  A future opt-in via a secret token may be
//! added for remote ops tooling, but requires explicit design.
//!
//! # Usage
//!
//! ```bash
//! tier1 diag           # pretty human-readable output
//! tier1 diag --json    # machine-readable JSON (same struct)
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
    pub fn skipped() -> Self {
        Self { status: ValidationStatus::Skipped, message: None }
    }
}

// ── Concurrent-limit snapshot ─────────────────────────────────────────────────

/// Configured concurrency limits for a tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyLimits {
    /// Global cap across all requests on this server instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<u32>,
    /// Per-upstream-host cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_host: Option<u32>,
    /// Per-customer-account cap (only applicable on private deployments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_account: Option<u32>,
}

// ── DiagReport ────────────────────────────────────────────────────────────────

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
    pub server_port: u16,
    /// Server identifier (colo code or operator label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    /// Developer / debug mode enabled.
    pub developer_mode: bool,
    /// Trace sink DSN if configured, or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_url: Option<String>,
    /// Whether a customer token is configured.  The token value is never
    /// included in the report — only its presence is recorded.
    pub customer_token_set: bool,

    // ── Tier 1 ────────────────────────────────────────────────────────────────
    /// Tier 1 is always builtin — this field is informational only.
    pub tier1: TierStatus,
    /// Tier 1 download concurrency limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier1_concurrency: Option<ConcurrencyLimits>,

    // ── Tier 2 ────────────────────────────────────────────────────────────────
    pub tier2: TierStatus,
    /// Handoff URL or DSN, if configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2_handoff: Option<String>,
    /// Validation result for the tier 2 handoff target.
    pub tier2_validation: Validation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2_concurrency: Option<ConcurrencyLimits>,

    // ── Tier 3 ────────────────────────────────────────────────────────────────
    pub tier3: TierStatus,
    /// Handoff URL or DSN, if configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier3_handoff: Option<String>,
    /// Validation result for the tier 3 handoff target.
    pub tier3_validation: Validation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier3_concurrency: Option<ConcurrencyLimits>,

    // ── Cache ─────────────────────────────────────────────────────────────────
    /// Human-readable summary of the cache configuration (backends, TTLs, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_config: Option<String>,
    /// Validation result for cache backend(s).
    pub cache_validation: Validation,

    // ── Account ───────────────────────────────────────────────────────────────
    /// Customer account identifier this deployment is running under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Validation result for account credentials / API key.
    pub account_validation: Validation,

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
}

// ── Collector ────────────────────────────────────────────────────────────────

/// Collect a diagnostic report for the current process environment.
///
/// All configuration is read from `AppConfig` — which itself was populated
/// from env vars at startup.  External connectivity checks (ping handoff
/// servers, connect to cache) are stubbed as `Skipped` until those subsystems
/// are wired up.
#[cfg(feature = "native")]
pub fn collect(cfg: &crate::config::AppConfig) -> DiagReport {
    // Tier 2
    let (tier2, tier2_handoff, tier2_validation) = match cfg.tier2_url.as_ref() {
        Some(url) => (TierStatus::Handoff, Some(url.clone()), Validation::skipped()),
        None      => (TierStatus::Missing, None,              Validation::not_configured()),
    };
    let tier2_concurrency = cfg.tier2_concurrency.map(|n| ConcurrencyLimits {
        global: Some(n), per_host: None, per_account: None,
    });

    // Tier 3
    let (tier3, tier3_handoff, tier3_validation) = match cfg.tier3_url.as_ref() {
        Some(url) => (TierStatus::Handoff, Some(url.clone()), Validation::skipped()),
        None      => (TierStatus::Missing, None,              Validation::not_configured()),
    };
    let tier3_concurrency = cfg.tier3_concurrency.map(|n| ConcurrencyLimits {
        global: Some(n), per_host: None, per_account: None,
    });

    // Tier 1 download concurrency
    let tier1_concurrency = cfg.download_concurrency.map(|n| ConcurrencyLimits {
        global: Some(n), per_host: None, per_account: None,
    });

    // Cache
    let (cache_config, cache_validation) = match cfg.cache_url.as_ref() {
        Some(dsn) => {
            let mut desc = dsn.clone();
            if let Some(max) = cfg.cache_max_items {
                desc = format!("{desc}  (max_items={max})");
            }
            (Some(desc), Validation::skipped()) // TODO: connect + ping
        }
        None => (None, Validation::not_configured()),
    };

    // Account / token
    let (account, account_validation) = match cfg.account_id.as_ref() {
        Some(id) => (Some(id.clone()), Validation::skipped()), // TODO: verify token
        None     => (None, Validation::not_configured()),
    };
    // customer_token present without account_id is a misconfiguration
    let account_validation = if cfg.customer_token.is_some() && cfg.account_id.is_none() {
        Validation::error("TBR_CUSTOMER_TOKEN is set but TBR_ACCOUNT_ID is missing")
    } else {
        account_validation
    };

    let build_timestamp = option_env!("TBR_BUILD_TIMESTAMP").map(str::to_owned);

    // Trace sink
    let trace_url = cfg.trace_url.clone();

    // Container / Docker image detection
    let container_image = detect_container_image();

    let healthy = !matches!(tier2, TierStatus::Error)
        && !matches!(tier3, TierStatus::Error)
        && !matches!(cache_validation.status, ValidationStatus::Error)
        && !matches!(account_validation.status, ValidationStatus::Error);

    DiagReport {
        runtime: RuntimeMode::Cli,
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_timestamp,
        server_port: cfg.port,
        server_id: cfg.server.clone(),
        developer_mode: cfg.developer_mode,
        trace_url,
        customer_token_set: cfg.customer_token.is_some(),
        tier1: TierStatus::Builtin,
        tier1_concurrency,
        tier2,
        tier2_handoff,
        tier2_validation,
        tier2_concurrency,
        tier3,
        tier3_handoff,
        tier3_validation,
        tier3_concurrency,
        cache_config,
        cache_validation,
        account,
        account_validation,
        healthy,
        container_image,
    }
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
        println!("Thumbrella Tier 1 — Diagnostics");
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
        println!("  server_id       : {}", self.server_id.as_deref().unwrap_or("—"));
        println!("  developer_mode  : {}", self.developer_mode);
        println!("  trace_url       : {}", self.trace_url.as_deref().unwrap_or("none"));
        println!("  customer_token  : {}", if self.customer_token_set { "set" } else { "—" });
        println!();

        println!("Tiers");
        print_tier("tier1", &self.tier1, None, &Validation::ok(), self.tier1_concurrency.as_ref());
        print_tier("tier2", &self.tier2, self.tier2_handoff.as_deref(), &self.tier2_validation, self.tier2_concurrency.as_ref());
        print_tier("tier3", &self.tier3, self.tier3_handoff.as_deref(), &self.tier3_validation, self.tier3_concurrency.as_ref());
        println!();

        println!("Cache");
        println!("  config          : {}", self.cache_config.as_deref().unwrap_or("—"));
        print_validation("  validation", &self.cache_validation);
        println!();

        println!("Account");
        println!("  id              : {}", self.account.as_deref().unwrap_or("—"));
        print_validation("  validation", &self.account_validation);
        println!();

        let status = if self.healthy { "OK ✓" } else { "DEGRADED ✗" };
        println!("Overall: {status}");
    }
}

fn print_tier(label: &str, status: &TierStatus, handoff: Option<&str>, validation: &Validation, concurrency: Option<&ConcurrencyLimits>) {
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
    if let Some(c) = concurrency {
        if let Some(n) = c.global {
            println!("    concurrency   : {n}");
        }
    }
    print_validation(&format!("    validation  "), validation);
}

fn print_validation(label: &str, v: &Validation) {
    let s = match v.status {
        ValidationStatus::Ok            => "ok",
        ValidationStatus::NotConfigured => "not configured",
        ValidationStatus::Error         => "error",
        ValidationStatus::Skipped       => "skipped",
    };
    if let Some(ref msg) = v.message {
        println!("{label}: {s} — {msg}");
    } else {
        println!("{label}: {s}");
    }
}
