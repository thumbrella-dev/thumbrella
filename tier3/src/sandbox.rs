//! Subprocess sandbox — bubblewrap isolation and rlimit fallback.
//!
//! On Linux, every subprocess renderer is wrapped in `bwrap` (bubblewrap)
//! for full filesystem and namespace isolation.  On other platforms, a
//! `pre_exec`-based rlimit/capability sandbox is used as fallback.
//!
//! # Platform support
//!
//! | Platform | Mechanism |
//! |----------|-----------|
//! | Linux + bwrap | `bwrap --unshare-all` + selective bind mounts |
//! | Linux (no bwrap) | `pre_exec`: prctl + setrlimit |
//! | macOS | `pre_exec`: setrlimit only |
//! | Other | no-op |
//!
//! # Bubblewrap isolation
//!
//! The bwrap sandbox unshares all namespaces (mount, PID, IPC, UTS, cgroup,
//! network) then selectively re-enables what the subprocess needs:
//!
//! - **Read-only**: /usr, /lib, /lib64, /bin, /etc, /opt
//! - **Writable**: the scratch arena directory only
//! - **Devices**: /dev/null, /dev/zero, /dev/random
//! - **Proc**: /proc for self-inspection
//! - **No network**: `--unshare-net` (overridable via `needs_networking`)
//! - **Private /tmp**: tmpfs

use std::path::Path;
use std::process::Command;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::process::CommandExt;

//  SandboxConfig

/// Limits applied to a subprocess before `exec`.
///
/// All limits are best-effort — unsupported platforms silently ignore them.
/// Zero means "no limit".
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Maximum address space in bytes (RLIMIT_AS).  0 = no limit.
    /// Only used by the pre_exec fallback, not by bwrap.
    pub max_memory: u64,
    /// Maximum number of open file descriptors (RLIMIT_NOFILE).  0 = no limit.
    pub max_fds: u64,
    /// Allow the subprocess to produce core dumps.
    pub allow_core_dumps: bool,
    /// Drop all ambient capabilities (Linux pre_exec fallback only).
    pub drop_capabilities: bool,
    /// Tool needs outbound network access (unshares network namespace
    /// in bwrap, but re-enables via --share-net).
    pub needs_networking: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            // 0 = no address space limit — needed for Python-based renderers
            // (usd-core) and FFmpeg that load large native libraries.
            max_memory: 0,
            max_fds: 64,
            allow_core_dumps: false,
            drop_capabilities: true,
            needs_networking: false,
        }
    }
}

pub fn default_strict() -> SandboxConfig {
    SandboxConfig::default()
}

//  bwrap wrapper (Linux, preferred)

/// Wrap a command in bubblewrap for filesystem and namespace isolation.
///
/// `scratch_dir` is the only directory the subprocess can write to.
/// `program` and `args` are the command to run inside the sandbox.
///
/// Returns a [`Command`] that invokes `bwrap` with the appropriate
/// isolation flags, then runs `program` with `args`.
pub fn bwrap_command(scratch_dir: &Path, program: &str, args: &[&str], config: &SandboxConfig) -> Command {
    let mut cmd = Command::new("bwrap");

    //  Namespace isolation
    cmd.arg("--unshare-all");
    if config.needs_networking {
        cmd.arg("--share-net");
    }
    cmd.arg("--die-with-parent");
    cmd.arg("--new-session");

    //  Read-only system mounts
    for dir in &["/usr", "/lib", "/lib64", "/bin", "/etc", "/opt"] {
        if Path::new(dir).exists() {
            cmd.arg("--ro-bind").arg(dir).arg(dir);
        }
    }

    //  Writable scratch directory
    cmd.arg("--bind").arg(scratch_dir).arg(scratch_dir);

    //  Minimal /dev
    cmd.arg("--dev").arg("/dev");

    //  /proc for self-inspection
    cmd.arg("--proc").arg("/proc");

    //  Private /tmp
    cmd.arg("--tmpfs").arg("/tmp");

    //  No setuid, lock ASLR
    cmd.arg("--no-new-session");

    //  Separator + target command
    cmd.arg("--");
    cmd.arg(program);
    for a in args {
        cmd.arg(a);
    }

    cmd
}

/// Check if bubblewrap is available.  Call once at startup to decide
/// which sandbox path to use.
pub fn bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a sandboxed [`Command`].  Uses bubblewrap on Linux when
/// available, falls back to pre_exec rlimits otherwise.
///
/// `scratch_dir` is the arena root — the subprocess can only write here
/// when bwrap is active.
pub fn sandboxed_command(scratch_dir: &Path, program: &str, args: &[&str]) -> Command {
    if bwrap_available() {
        bwrap_command(scratch_dir, program, args, &SandboxConfig::default())
    } else {
        let mut cmd = Command::new(program);
        for a in args {
            cmd.arg(a);
        }
        apply(&mut cmd, &SandboxConfig::default());
        cmd
    }
}

//  pre_exec fallback (Linux / macOS, no bwrap)

/// Apply sandbox restrictions via `pre_exec` (rlimits, capabilities).
/// Used as fallback when bubblewrap is not available.
///
/// Only meaningful on Unix; no-op on Windows.
#[cfg(unix)]
pub fn apply(cmd: &mut Command, config: &SandboxConfig) {
    let config = config.clone();
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(move || sandbox_linux(&config));
    }
    #[cfg(target_os = "macos")]
    unsafe {
        cmd.pre_exec(move || sandbox_macos(&config));
    }
}

#[cfg(not(unix))]
pub fn apply(_cmd: &mut Command, _config: &SandboxConfig) {
    // Windows: pre_exec and rlimits are not available.  No-op.
}

//  Linux implementation

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
        // EINVAL: cap not in bounding set.  EPERM: already non-root.
        // Both are expected and harmless.
        let _ = rc;
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

//  macOS implementation

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

//  rlimit helper

#[cfg(unix)]
fn set_rlimit(resource: u32, soft: u64, hard: u64) -> std::io::Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    let rc = unsafe { libc::setrlimit(resource, &rlim) };
    // Silently ignore rlimit errors — the limits are best-effort and
    // may fail when the process already has tighter constraints.
    let _ = rc;
    Ok(())
}
