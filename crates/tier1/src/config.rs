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
//! | `TBR_PORT`                 | 8000    | HTTP listener port                               |
//! | `TBR_SERVER`               | —       | Short server/colo identifier for traces          |
//! | `TBR_DEVELOPER_MODE`       | false   | Verbose debug output in API responses            |
//! | `TBR_TIER2`                | —       | Tier-2 URL with optional `#code` handoff secret  |
//! | `TBR_TIER3`                | —       | Tier-3 URL with optional `#code` handoff secret  |
//! | `TBR_HANDOFF`              | —       | Shared secret this server accepts on `/handoff`  |
//! | `TBR_CACHE`                | —       | Cache backend DSN — `sqlite:<path>`, …          |
//! | `TBR_CACHE_MAX_ITEMS`      | —       | Max cache entries (backend-specific meaning)     |
//! | `TBR_TRACE`                | —       | Trace sink DSN — `ndjson:<path>`, …             |

// ── AppConfig ─────────────────────────────────────────────────────────────────

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

    // ── Handoff tiers ─────────────────────────────────────────────────────────
    /// URL of the tier-2 handoff server (`TBR_TIER2`).
    pub tier2_url: Option<String>,
    /// Optional per-tier handoff code parsed from `TBR_TIER2` URL fragment.
    pub tier2_code: Option<String>,
    /// URL of the tier-3 handoff server (`TBR_TIER3`).
    pub tier3_url: Option<String>,
    /// Optional per-tier handoff code parsed from `TBR_TIER3` URL fragment.
    pub tier3_code: Option<String>,
    /// Shared secret accepted in `x-tbr-handoff-code` for `/handoff` calls.
    /// If `None`, this server does not accept handoff requests.
    pub handoff_accept: Option<String>,

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

}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 8000,
            server: None,
            developer_mode: false,
            tier2_url: None,
            tier2_code: None,
            tier3_url: None,
            tier3_code: None,
            handoff_accept: None,
            cache_url: None,
            cache_max_items: None,
            trace_url: None,
        }
    }
}

impl AppConfig {
    /// Build config from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let (tier2_url, tier2_code) = parse_handoff_target(env_opt_string("TBR_TIER2"));
        let (tier3_url, tier3_code) = parse_handoff_target(env_opt_string("TBR_TIER3"));
        Self {
            port:                 env_u16("TBR_PORT", 8000),
            server:               std::env::var("TBR_SERVER").ok(),
            developer_mode:       env_bool("TBR_DEVELOPER_MODE", false),
            tier2_url,
            tier2_code,
            tier3_url,
            tier3_code,
            handoff_accept:       env_opt_string("TBR_HANDOFF"),
            cache_url:            std::env::var("TBR_CACHE").ok(),
            cache_max_items:      env_opt_u32("TBR_CACHE_MAX_ITEMS"),
            trace_url:            std::env::var("TBR_TRACE").ok(),
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

fn env_opt_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn env_opt_string(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn parse_handoff_target(raw: Option<String>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw else { return (None, None); };
    let (base, code) = match raw.split_once('#') {
        Some((u, c)) => (u.trim(), Some(c.trim())),
        None => (raw.trim(), None),
    };
    let url = if base.is_empty() { None } else { Some(base.to_string()) };
    let code = code.and_then(|c| if c.is_empty() { None } else { Some(c.to_string()) });
    (url, code)
}
