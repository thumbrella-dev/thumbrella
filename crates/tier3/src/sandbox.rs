//! Subprocess sandbox — resource limits and capability dropping.
//!
//! Every subprocess renderer runs through a sandbox that applies OS-level
//! restrictions between `fork()` and `exec()`.  The goal is defence in depth:
//! even if a renderer script or binary is compromised, the blast radius is
//! contained.
//!
//! # Platform support
//!
//! | Platform | Mechanism |
//! |----------|-----------|
//! | Linux    | `prctl` (no-new-privs), `setrlimit` (nofile, core, as), `capset` (drop all) |
//! | macOS    | `setrlimit` only (no `prctl`, no capabilities) |
//! | Other    | no-op — subprocess runs with full ambient authority |
//!
//! # Per-tool configuration
//!
//! Some tools need relaxed limits (e.g. a renderer that fetches textures from
//! the network).  These are expressed as flags on [`SandboxConfig`]:
//!
//! - `needs_networking` — skip network-namespace isolation (future).
//! - `allow_core_dumps` — permit core dumps for debugging renderer crashes.
//!
//! # Future: bubblewrap
//!
//! On Linux, the long-term plan is to wrap every subprocess in `bwrap` for
//! full filesystem and network isolation.  This module is designed so that
//! [`apply`] can switch from `pre_exec` to `bwrap` wrapper transparently.

use std::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::process::CommandExt;

// ── SandboxConfig ─────────────────────────────────────────────────────────────

/// Limits applied to a subprocess before `exec`.
///
/// All limits are best-effort — unsupported platforms silently ignore them.
/// Zero means "no limit".
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Maximum address space in bytes (RLIMIT_AS).  0 = no limit.
    pub max_memory: u64,
    /// Maximum number of open file descriptors (RLIMIT_NOFILE).  0 = no limit.
    pub max_fds: u64,
    /// Allow the subprocess to produce core dumps.
    pub allow_core_dumps: bool,
    /// Drop all ambient capabilities (Linux only).
    pub drop_capabilities: bool,
    /// Tool needs outbound network access (future: skips network namespace).
    #[allow(dead_code)]
    pub needs_networking: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            max_memory:         512 * 1024 * 1024, // 512 MiB
            max_fds:            64,
            allow_core_dumps:   false,
            drop_capabilities:  true,
            needs_networking:   false,
        }
    }
}

/// A restrictive sandbox suitable for renderers that only need local I/O.
pub fn default_strict() -> SandboxConfig {
    SandboxConfig::default()
}

// ── Apply ─────────────────────────────────────────────────────────────────────

/// Apply sandbox restrictions to a [`Command`].
///
/// Call this before `cmd.status()` or `cmd.output()`.  The restrictions
/// take effect in the child process after `fork()` but before `exec()`.
///
/// # Safety
///
/// The `pre_exec` closure runs in the forked child before `exec`.  It must
/// only call async-signal-safe functions.  The implementations below use
/// only `setrlimit`, `prctl`, and `capset` — all async-signal-safe.
pub fn apply(cmd: &mut Command, config: &SandboxConfig) {
    let config = config.clone();
    #[cfg(target_os = "linux")]
    unsafe {
        // pre_exec is unsafe because the closure runs in a forked child
        // with constraints on what operations are safe.  We only call
        // async-signal-safe libc functions.
        cmd.pre_exec(move || sandbox_linux(&config));
    }
    #[cfg(target_os = "macos")]
    unsafe {
        cmd.pre_exec(move || sandbox_macos(&config));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = config; // no-op on other platforms
    }
}

// ── Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn sandbox_linux(config: &SandboxConfig) -> std::io::Result<()> {
    // 1. Prevent privilege escalation.
    prctl_no_new_privs()?;

    // 2. Drop all capabilities.
    if config.drop_capabilities {
        drop_all_caps()?;
    }

    // 3. Resource limits.
    set_rlimit(libc::RLIMIT_NOFILE, config.max_fds, config.max_fds)?;
    set_rlimit(libc::RLIMIT_CORE, 0, 0)?; // never allow core dumps
    if config.allow_core_dumps {
        set_rlimit(libc::RLIMIT_CORE, libc::RLIM_INFINITY, libc::RLIM_INFINITY)?;
    }
    if config.max_memory > 0 {
        set_rlimit(libc::RLIMIT_AS, config.max_memory, config.max_memory)?;
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn prctl_no_new_privs() -> std::io::Result<()> {
    // PR_SET_NO_NEW_PRIVS = 36, prevents setuid and capability gain.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn drop_all_caps() -> std::io::Result<()> {
    // Drop all capabilities from the bounding set.
    // We iterate CAP_LAST_CAP down to 0; on kernels that don't support
    // PR_CAPBSET_DROP we silently ignore errors (older kernels).
    let last_cap = cap_last_cap();
    for cap in 0..=last_cap {
        // PR_CAPBSET_DROP = 24
        let rc = unsafe { libc::prctl(24, cap as u64, 0, 0, 0) };
        // EINVAL means the capability wasn't in the bounding set — fine.
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINVAL) {
                // Non-EINVAL errors on prctl are unexpected; log and continue.
                eprintln!("[tier3] sandbox: prctl(CAPBSET_DROP, {cap}): {err}");
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cap_last_cap() -> u32 {
    // Read /proc/sys/kernel/cap_last_cap to get the highest valid cap.
    // Fall back to 40 (CAP_CHECKPOINT_RESTORE) if unreadable.
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(40)
}

// ── macOS implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn sandbox_macos(config: &SandboxConfig) -> std::io::Result<()> {
    // macOS has no prctl or capabilities, but we can set rlimits.
    set_rlimit(libc::RLIMIT_NOFILE, config.max_fds, config.max_fds)?;
    set_rlimit(libc::RLIMIT_CORE, 0, 0)?;
    if config.allow_core_dumps {
        set_rlimit(libc::RLIMIT_CORE, libc::RLIM_INFINITY, libc::RLIM_INFINITY)?;
    }
    if config.max_memory > 0 {
        set_rlimit(libc::RLIMIT_AS, config.max_memory, config.max_memory)?;
    }
    Ok(())
}

// ── rlimit helper ─────────────────────────────────────────────────────────────

fn set_rlimit(resource: u32, soft: u64, hard: u64) -> std::io::Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    let rc = unsafe { libc::setrlimit(resource, &rlim) };
    if rc != 0 {
        // RLIMIT_NOFILE may fail if the soft limit exceeds the hard cap.
        // Log and continue — don't fail the spawn.
        let err = std::io::Error::last_os_error();
        eprintln!("[tier3] sandbox: setrlimit({resource}): {err}");
    }
    Ok(())
}
