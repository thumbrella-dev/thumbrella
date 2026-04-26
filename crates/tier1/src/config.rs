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
//! | `TBR_TIER2_URL`            | —       | Handoff URL for tier-2 rendering                 |
//! | `TBR_TIER3_URL`            | —       | Handoff URL for tier-3 rendering                 |
//! | `TBR_CACHE_URL`            | —       | Cache backend DSN (redis://, kv://, file://, …)  |
//! | `TBR_CACHE_MAX_ITEMS`      | —       | Max cache entries (backend-specific meaning)     |
//! | `TBR_TRACE_SINK`           | stdout  | Where traces are emitted; see [`TraceSink`]      |
//! | `TBR_CUSTOMER_TOKEN`       | —       | Customer API token for paid/hosted builds        |
//! | `TBR_ACCOUNT_ID`           | —       | Customer account identifier (billing/quota)      |
//! | `TBR_DOWNLOAD_CONCURRENCY` | —       | Max simultaneous upstream downloads              |
//! | `TBR_TIER2_CONCURRENCY`    | —       | Max simultaneous tier-2 handoff requests         |
//! | `TBR_TIER3_CONCURRENCY`    | —       | Max simultaneous tier-3 handoff requests         |

// ── TraceSink ────────────────────────────────────────────────────────────────

/// Where per-item trace records are emitted.
///
/// Parsed from `TBR_TRACE_SINK`.  Unrecognised values are treated as `Stdout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceSink {
    /// Write JSON trace records to stdout (default; works everywhere).
    Stdout,
    /// Append JSON trace records to a local file path.
    File(String),
    /// Push to a Grafana / Loki push endpoint (future).
    Grafana(String),
    /// Use Cloudflare's native analytics pipeline (Workers only; no-op on native).
    Cloudflare,
}

impl TraceSink {
    fn from_env() -> Self {
        match std::env::var("TBR_TRACE_SINK").as_deref() {
            Ok("stdout") | Err(_) => Self::Stdout,
            Ok("cloudflare")      => Self::Cloudflare,
            Ok(v) if v.starts_with("grafana://") => Self::Grafana(v.to_owned()),
            Ok(v)                 => Self::File(v.to_owned()),
        }
    }

    /// Human-readable config string for diagnostics.
    pub fn display(&self) -> String {
        match self {
            Self::Stdout       => "stdout".to_string(),
            Self::Cloudflare   => "cloudflare".to_string(),
            Self::File(p)      => format!("file:{p}"),
            Self::Grafana(url) => format!("grafana:{url}"),
        }
    }
}

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
    /// URL of the tier-2 handoff server (`TBR_TIER2_URL`).
    pub tier2_url: Option<String>,
    /// URL of the tier-3 handoff server (`TBR_TIER3_URL`).
    pub tier3_url: Option<String>,

    // ── Cache ────────────────────────────────────────────────────────────────
    /// Cache backend DSN (`TBR_CACHE_URL`).  Scheme determines backend type:
    /// `redis://`, `kv://` (Cloudflare KV), `file://`, etc.
    pub cache_url: Option<String>,
    /// Maximum number of cache entries (`TBR_CACHE_MAX_ITEMS`).
    pub cache_max_items: Option<u32>,

    // ── Trace sink ────────────────────────────────────────────────────────────
    /// Where per-item trace records are emitted.
    pub trace_sink: TraceSink,

    // ── Account / auth ────────────────────────────────────────────────────────
    /// Customer API token for paid/hosted builds (`TBR_CUSTOMER_TOKEN`).
    /// Required when billing or quota enforcement is active; optional otherwise.
    pub customer_token: Option<String>,
    /// Customer account identifier for billing and quota attribution (`TBR_ACCOUNT_ID`).
    pub account_id: Option<String>,

    // ── Concurrency limits ────────────────────────────────────────────────────
    /// Max simultaneous upstream source downloads (`TBR_DOWNLOAD_CONCURRENCY`).
    pub download_concurrency: Option<u32>,
    /// Max simultaneous tier-2 handoff requests in flight (`TBR_TIER2_CONCURRENCY`).
    pub tier2_concurrency: Option<u32>,
    /// Max simultaneous tier-3 handoff requests in flight (`TBR_TIER3_CONCURRENCY`).
    pub tier3_concurrency: Option<u32>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 8000,
            server: None,
            developer_mode: false,
            tier2_url: None,
            tier3_url: None,
            cache_url: None,
            cache_max_items: None,
            trace_sink: TraceSink::Stdout,
            customer_token: None,
            account_id: None,
            download_concurrency: None,
            tier2_concurrency: None,
            tier3_concurrency: None,
        }
    }
}

impl AppConfig {
    /// Build config from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        Self {
            port:                 env_u16("TBR_PORT", 8000),
            server:               std::env::var("TBR_SERVER").ok(),
            developer_mode:       env_bool("TBR_DEVELOPER_MODE", false),
            tier2_url:            std::env::var("TBR_TIER2_URL").ok(),
            tier3_url:            std::env::var("TBR_TIER3_URL").ok(),
            cache_url:            std::env::var("TBR_CACHE_URL").ok(),
            cache_max_items:      env_opt_u32("TBR_CACHE_MAX_ITEMS"),
            trace_sink:           TraceSink::from_env(),
            customer_token:       std::env::var("TBR_CUSTOMER_TOKEN").ok(),
            account_id:           std::env::var("TBR_ACCOUNT_ID").ok(),
            download_concurrency: env_opt_u32("TBR_DOWNLOAD_CONCURRENCY"),
            tier2_concurrency:    env_opt_u32("TBR_TIER2_CONCURRENCY"),
            tier3_concurrency:    env_opt_u32("TBR_TIER3_CONCURRENCY"),
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
