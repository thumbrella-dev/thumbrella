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
//! | `TBR_ALLOW_LOCAL`          | false   | Accept `file://` URLs, bare paths, and localhost |
//! | `TBR_SCRATCH`              | $TMPDIR/thumbrella | Scratch root for tier3 CLI tool staging   |
//! | `TBR_TIER2`                | -       | Tier-2 connect string (URL + optional headers)   |
//! | `TBR_TIER3`                | -       | Tier-3 connect string (URL + optional headers)   |
//! | `TBR_HANDSHAKE`            | -       | Shared secret required on all endpoints          |
//! | `TBR_CACHE`                | -       | Cache backend DSN - `sqlite:<path>`, …           |
//! | `TBR_TRACE`                | -       | Trace sink DSN - `ndjson:<path>`, …              |
//! | `TBR_LOG`                  | standard| Output level: `standard`, `minimal`, `full`      |
//!
//! # Connect-string syntax
//!
//! `TBR_TIER2` and `TBR_TIER3` use the same connect-string format as
//! `TBR_CONNECT` in the TypeScript client:
//!
//! - Plain URL: `http://tier2:8000`
//! - URL with explicit headers: `http://tier2:8000,x-tbr-handshake=secret`
//! - URL with auth token: `http://tier2:8000,tbr_s_AbCd...`
//!   Bare values starting with `tbr_[a-z]_` are sent as `Authorization: Bearer`.
//! - URL with handshake shorthand: `http://tier2:8000,mysecret`
//!   Any bare value that does *not* match the auth-token prefix is treated as
//!   an `x-tbr-handshake` header.
//! - Bare auth token without URL: `tbr_s_AbCd...` → `Authorization: Bearer`
//! - Bare handshake without URL: `mysecret` → `x-tbr-handshake: mysecret`
//! - Backward-compat `#` fragment: `http://tier2:8000#secret`
//!   (The fragment is treated as an `x-tbr-handshake` header value.)

//  AppConfig

use crate::connect::{ConnectTarget, parse_connect_target};

/// Full runtime configuration for a tier-1 server instance.
///
/// Constructed once at startup via [`AppConfig::from_env`] and passed to route
/// handlers, the diagnostic collector, and any background workers.
#[derive(Debug, Clone)]
pub struct AppConfig {
    //  Server identity
    /// HTTP listener port.
    pub port: u16,
    /// Short server identifier included in trace records.
    ///
    /// Use a Cloudflare colo code (e.g. `"SJC"`) or an operator-assigned label
    /// (e.g. `"prod-1"`).
    pub server: Option<String>,
    /// Allow `file://`, bare paths, and localhost/private-network URLs.
    ///
    /// When `true`, callers may pass `file:///path/to/file`, bare absolute
    /// paths, or `localhost` / private-IP URLs.  **Only enable in trusted
    /// environments** - any caller can read any file the server process has
    /// permission to open.
    pub allow_local: bool,
    /// Root directory for temporary scratch space used by tier3 CLI tool
    /// staging.  Defaults to `$TMPDIR/thumbrella` (or `/tmp/thumbrella`).
    pub scratch_dir: String,

    //  Handoff tiers
    /// Tier-2 connect target parsed from `TBR_TIER2`.
    pub tier2: ConnectTarget,
    /// Tier-3 connect target parsed from `TBR_TIER3`.
    pub tier3: ConnectTarget,
    /// Shared secret required on all endpoints when set.
    /// If `None`, the server is publicly accessible.
    pub handshake: Option<String>,

    //  Cache
    /// Cache backend DSN (`TBR_CACHE`).  Scheme determines backend type:
    /// `mem:`, `sqlite:`, `none:`.
    pub cache_url: Option<String>,
    /// Maximum server-side cache TTL in seconds.  Upstream `max-age` values
    /// are capped at this duration.  Default: 7 days (604800).
    pub cache_max_ttl_secs: u64,
    /// Default cache TTL when upstream provides no freshness hints.
    /// Default: 1 hour (3600).
    pub cache_default_ttl_secs: u64,

    //  Trace sink
    /// Trace sink DSN (`TBR_TRACE`).  Scheme determines backend type:
    /// `ndjson:<path>`, etc.  `None` disables trace logging.
    pub trace_url: Option<String>,

    //  Fetch protection (hardcoded defaults - not exposed as env vars)
    /// URL failure debounce window in seconds.
    pub failure_ttl: u32,
    /// Default origin back-off TTL when no `Retry-After` header is present.
    pub backoff_default: u32,
    /// Maximum origin back-off TTL cap in seconds.
    pub backoff_ceiling: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 3114,
            server: None,
            allow_local: false,
            scratch_dir: default_scratch_dir(),
            tier2: ConnectTarget::default(),
            tier3: ConnectTarget::default(),
            handshake: None,
            cache_url: None,
            cache_max_ttl_secs: 604_800,   // 7 days
            cache_default_ttl_secs: 3_600, // 1 hour
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
            port: env_u16("TBR_PORT", 3114),
            allow_local: env_bool("TBR_ALLOW_LOCAL", false),
            scratch_dir: env_scratch("TBR_SCRATCH"),
            tier2,
            tier3,
            handshake: env_opt_string("TBR_HANDSHAKE"),
            server: env_opt_string("TBR_SERVER"),
            cache_url: std::env::var("TBR_CACHE").ok(),
            cache_max_ttl_secs: env_opt_u32("TBR_CACHE_MAX_TTL").unwrap_or(604_800) as u64,
            cache_default_ttl_secs: env_opt_u32("TBR_CACHE_DEFAULT_TTL").unwrap_or(3_600) as u64,
            trace_url: std::env::var("TBR_TRACE").ok(),
            // Hardcoded - not exposed as env vars.
            failure_ttl: 5,
            backoff_default: 60,
            backoff_ceiling: 3_600,
        }
    }
}

//  Env helpers

fn env_u16(name: &str, default: u16) -> u16 {
    match std::env::var(name) {
        Ok(v) => v.trim().parse().unwrap_or_else(|_| {
            crate::ux::get().fatal(
                &format!("{name} is set to \"{v}\", which is not a valid port number"),
                &format!(
                    "set {name} to a number between 1 and 65535, or unset it to use the default ({default})"
                ),
            );
        }),
        Err(_) => default,
    }
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("1" | "true" | "yes") => true,
        Ok("0" | "false" | "no") => false,
        Ok(other) => {
            crate::ux::get().fatal(
                &format!("{name} is set to \"{other}\", which is not a valid boolean"),
                &format!(
                    "set {name} to true/false/1/0/yes/no, or unset it to use the default ({})",
                    if default { "true" } else { "false" }
                ),
            );
        }
        Err(_) => default,
    }
}

fn env_opt_u32(name: &str) -> Option<u32> {
    match std::env::var(name) {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(trimmed.parse().unwrap_or_else(|_| {
                crate::ux::get().fatal(
                    &format!("{name} is set to \"{trimmed}\", which is not a valid number"),
                    &format!("set {name} to a number, or unset it"),
                );
            }))
        }
        Err(_) => None,
    }
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
