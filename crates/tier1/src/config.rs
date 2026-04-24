//! Runtime configuration for the native server.
//!
//! Values are read from environment variables at startup.  The compiled-in
//! defaults match the canonical thumbnail profile so a zero-config deployment
//! just works.

/// Runtime configuration for the native tier-1 server.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Port to bind the HTTP listener on.
    pub port: u16,
    /// Enable verbose developer/debug output in API responses.
    pub developer_mode: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 8000,
            developer_mode: false,
        }
    }
}

impl AppConfig {
    /// Build config from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            port: env_u16("TBR_PORT", defaults.port),
            developer_mode: env_bool("TBR_DEVELOPER_MODE", defaults.developer_mode),
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
