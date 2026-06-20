//! Runtime configuration for the native server.
//!
//! All values are read from environment variables at startup.  Every field is
//! optional and has a safe built-in default so a zero-config deployment works
//! out of the box.
//!
//! # Environment variables
//!
//! | Variable                   | Default | Description                                      |
//! |----------------------------|---------|--------------------------------------------------|
//! | `TBR_PORT`                 | 3114    | HTTP listener port                               |
//! | `TBR_SERVER`               | —       | Short server/colo identifier for traces          |
//! | `TBR_DEVELOPER_MODE`       | false   | Verbose debug output in API responses            |
//! | `TBR_ALLOW_FILES`          | false   | Accept `file://` URLs and bare absolute paths    |
//! | `TBR_SCRATCH`              | $TMPDIR/thumbrella | Scratch root for tier3 CLI tool staging   |
//! | `TBR_TIER2`                | —       | Tier-2 connect string (URL + optional headers)   |
//! | `TBR_TIER3`                | —       | Tier-3 connect string (URL + optional headers)   |
//! | `TBR_HANDSHAKE`           | —       | Shared secret required on all endpoints       |
//! | `TBR_CACHE`                | —       | Cache backend DSN — `sqlite:<path>`, …          |
//! | `TBR_CACHE_MAX_ITEMS`      | —       | Max cache entries (backend-specific meaning)     |
//! | `TBR_TRACE`                | —       | Trace sink DSN — `ndjson:<path>`, …             |
//! | `TBR_FAILURE_TTL`          | 5       | URL failure debounce window (seconds)            |
//! | `TBR_BACKOFF_DEFAULT`      | 60      | Origin back-off TTL when no `Retry-After` header |
//! | `TBR_BACKOFF_CEILING`      | 3600    | Origin back-off maximum TTL cap (seconds)        |
//!
//! # Connect-string syntax
//!
//! `TBR_TIER2` and `TBR_TIER3` use the same connect-string format as
//! `TBR_CONNECT` in the TypeScript client:
//!
//! - Plain URL: `http://tier2:8000`
//! - URL with headers: `http://tier2:8000,x-tbr-handshake=secret`
//! - URL with Bearer token: `http://tier2:8000,token`
//! - Backward-compat `#` fragment: `http://tier2:8000#secret`
//!   (The fragment is treated as an `x-tbr-handshake` header value.)

// ── AppConfig ─────────────────────────────────────────────────────────────────

use crate::connect::{ConnectTarget, parse_connect_target};

/// Full runtime configuration for a tier-1 server instance.
///
/// Constructed once at startup via [`AppConfig::from_env`] and passed to route
/// handlers, the diagnostic collector, and any background workers.
#[derive(Debug, Clone)]
pub struct AppConfig {
    // ── Server identity ───────────────────────────────────────────────────────
    /// HTTP listener port.
    pub port: u16,
    /// Short server identifier included in trace records.
    ///
    /// Use a Cloudflare colo code (e.g. `"SJC"`) or an operator-assigned label
    /// (e.g. `"prod-1"`).
    pub server: Option<String>,
    /// Emit verbose debug data in API responses.
    pub developer_mode: bool,
    /// Allow `file://` URLs and bare absolute paths in HTTP endpoint requests.
    ///
    /// When `true`, callers may pass `file:///path/to/file` or a bare absolute
    /// path such as `/data/image.png` and the server will read it directly from
    /// the local filesystem.  **Only enable in trusted environments** — any
    /// caller can read any file the server process has permission to open.
    pub allow_local: bool,
    /// Root directory for temporary scratch space used by tier3 CLI tool
    /// staging.  Defaults to `$TMPDIR/thumbrella` (or `/tmp/thumbrella`).
    pub scratch_dir: String,

    // ── Handoff tiers ─────────────────────────────────────────────────────────
    /// Tier-2 connect target parsed from `TBR_TIER2`.
    pub tier2: ConnectTarget,
    /// Tier-3 connect target parsed from `TBR_TIER3`.
    pub tier3: ConnectTarget,
    /// Shared secret required on all endpoints when set.
    /// If `None`, the server is publicly accessible.
    pub handshake: Option<String>,

    // ── Cache ────────────────────────────────────────────────────────────────
    /// Cache backend DSN (`TBR_CACHE`).  Scheme determines backend type:
    /// `sqlite:`, etc.
    pub cache_url: Option<String>,
    /// Maximum number of cache entries (`TBR_CACHE_MAX_ITEMS`).
    pub cache_max_items: Option<u32>,

    // ── Trace sink ────────────────────────────────────────────────────────────
    /// Trace sink DSN (`TBR_TRACE`).  Scheme determines backend type:
    /// `ndjson:<path>`, etc.  `None` disables trace logging.
    pub trace_url: Option<String>,

    // ── Fetch protection ──────────────────────────────────────────────────────
    /// URL failure debounce window in seconds (`TBR_FAILURE_TTL`). Default: 5.
    pub failure_ttl: u32,
    /// Default origin back-off TTL when no `Retry-After` header is present
    /// (`TBR_BACKOFF_DEFAULT`). Default: 60.
    pub backoff_default: u32,
    /// Maximum origin back-off TTL cap in seconds (`TBR_BACKOFF_CEILING`).
    /// Default: 3600.
    pub backoff_ceiling: u32,

}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 3114,
            server: None,
            developer_mode: false,
            allow_local: false,
            scratch_dir: default_scratch_dir(),
            tier2: ConnectTarget::default(),
            tier3: ConnectTarget::default(),
            handshake: None,
            cache_url: None,
            cache_max_items: None,
            trace_url: None,
            failure_ttl: 5,
            backoff_default: 60,
            backoff_ceiling: 3_600,
        }
    }
}

impl AppConfig {
    /// Build config from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let tier2 = parse_connect_target(env_opt_string("TBR_TIER2"));
        let tier3 = parse_connect_target(env_opt_string("TBR_TIER3"));
        Self {
            port:                 env_u16("TBR_PORT", 3114),
            server:               std::env::var("TBR_SERVER").ok(),
            developer_mode:       env_bool("TBR_DEVELOPER_MODE", false),
            allow_local:          env_bool("TBR_ALLOW_FILES", false),
            scratch_dir:          env_scratch("TBR_SCRATCH"),
            tier2,
            tier3,
            handshake:          env_opt_string("TBR_HANDSHAKE"),
            cache_url:            std::env::var("TBR_CACHE").ok(),
            cache_max_items:      env_opt_u32("TBR_CACHE_MAX_ITEMS"),
            trace_url:            std::env::var("TBR_TRACE").ok(),
            failure_ttl:          env_u32("TBR_FAILURE_TTL", 5),
            backoff_default:      env_u32("TBR_BACKOFF_DEFAULT", 60),
            backoff_ceiling:      env_u32("TBR_BACKOFF_CEILING", 3_600),
        }
    }
}

// ── Env helpers ───────────────────────────────────────────────────────────────

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("1" | "true" | "yes") => true,
        Ok("0" | "false" | "no") => false,
        _ => default,
    }
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_opt_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn env_opt_string(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn default_scratch_dir() -> String {
    let tmp = std::env::var("TMPDIR")
        .or_else(|_| std::env::var("TMP"))
        .or_else(|_| std::env::var("TEMP"))
        .unwrap_or_else(|_| "/tmp".to_string());
    format!("{}/thumbrella", tmp.trim_end_matches('/'))
}

fn env_scratch(name: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_scratch_dir)
}
