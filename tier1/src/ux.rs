//! Server UX - startup banner, request logging, hints, and error formatting.
//!
//! All intentional user-facing output flows through this module.  Raw
//! tracing / eprintln is suppressed by default and only enabled when
//! `TBR_LOG=full`.
//!
//! # Output levels
//!
//! | `TBR_LOG`   | Banner | Request log | Hints | ffmpeg / rust logs |
//! |-------------|--------|-------------|-------|--------------------|
//! | `standard`  | yes    | yes         | yes   | no                 |
//! | `minimal`   | no     | yes         | no    | no                 |
//! | `full`      | yes    | yes         | yes   | yes                |
//!
//! `NO_COLOR=1` disables ANSI colour codes (standard widely-supported convention).
//!
//! Hints are shown in `standard` and `full` modes, suppressed in `minimal`.

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

//  Once-only warnings

/// Flags that fire exactly once per process lifetime.
static WARNED_FILE_URL: AtomicBool = AtomicBool::new(false);
static WARNED_LOCALHOST: AtomicBool = AtomicBool::new(false);

/// Warn once about a denied `file://` URL request.
pub fn warn_file_url_denied() {
    if !WARNED_FILE_URL.swap(true, Ordering::Relaxed) {
        let ux = get();
        ux.warn(
            "a file:// URL was requested, but local file access is disabled",
            "set TBR_ALLOW_LOCAL=true to enable file://, local-path, and localhost URLs",
        );
    }
}

/// Warn once about a denied localhost / private-network URL request.
pub fn warn_localhost_denied() {
    if !WARNED_LOCALHOST.swap(true, Ordering::Relaxed) {
        let ux = get();
        ux.warn(
            "a localhost or private-network URL was requested, but is denied by default",
            "set TBR_ALLOW_LOCAL=true to allow these URLs",
        );
    }
}

//  Startup tips

/// Short extra `#` lines queued by tier binaries to show in the startup
/// banner (e.g. a cloud nudge).  Printed once, gated by the same hint
/// setting as the banner (`standard`/`full`).
static STARTUP_TIPS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// Queue a short line (without the `  # ` prefix) for the startup banner.
pub fn add_startup_tip(tip: impl Into<String>) {
    STARTUP_TIPS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push(tip.into());
}

/// Drain and return queued startup tips.  Printed by the serve banner and
/// the `check` report, whichever runs first in this process.
pub fn drain_startup_tips() -> Vec<String> {
    match STARTUP_TIPS.get() {
        Some(m) => std::mem::take(&mut *m.lock().unwrap()),
        None => Vec::new(),
    }
}

//  OutputStyle

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStyle {
    /// Full output - banner, request log, hints, AND raw tracing/ffmpeg logs.
    Full,
    /// Default - banner, request log, hints.  No raw logs.
    Standard,
    /// Sparse - request log only, no banner, no hints, no raw logs.
    Minimal,
}

impl OutputStyle {
    pub fn from_env() -> Self {
        match std::env::var("TBR_LOG").as_deref() {
            Ok("full") => Self::Full,
            Ok("minimal") => Self::Minimal,
            _ => Self::Standard,
        }
    }

    pub fn show_banner(self) -> bool {
        matches!(self, Self::Full | Self::Standard)
    }
    pub fn show_hints(self) -> bool {
        self.show_banner()
    }
    pub fn show_raw_logs(self) -> bool {
        matches!(self, Self::Full)
    }
}

//  Colour helpers

fn use_colour() -> bool {
    !matches!(
        std::env::var("NO_COLOR").as_deref(),
        Ok(v) if !v.is_empty()
    )
}

/// Public helper so other modules can check colour state without
/// reaching into the `Ux` singleton.
pub fn colour_enabled() -> bool {
    use_colour()
}

struct Colour;

impl Colour {
    fn green(s: &str) -> String {
        if use_colour() { format!("\x1b[32m{s}\x1b[0m") } else { s.to_string() }
    }
    fn red(s: &str) -> String {
        if use_colour() { format!("\x1b[31m{s}\x1b[0m") } else { s.to_string() }
    }
    fn yellow(s: &str) -> String {
        if use_colour() { format!("\x1b[33m{s}\x1b[0m") } else { s.to_string() }
    }
    fn cyan(s: &str) -> String {
        if use_colour() { format!("\x1b[36m{s}\x1b[0m") } else { s.to_string() }
    }
    fn magenta(s: &str) -> String {
        if use_colour() { format!("\x1b[35m{s}\x1b[0m") } else { s.to_string() }
    }
    fn dim(s: &str) -> String {
        if use_colour() { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() }
    }
    fn bold(s: &str) -> String {
        if use_colour() { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() }
    }
}

//  Global UX instance

static UX: OnceLock<Ux> = OnceLock::new();

pub fn init() -> &'static Ux {
    UX.get_or_init(|| Ux {
        style: OutputStyle::from_env(),
    })
}

pub fn get() -> &'static Ux {
    UX.get().expect("ux::init() must be called before ux::get()")
}

/// True when raw debug/tracing output should be emitted.
/// Renderers and decode paths gate their eprintln! calls behind this.
pub fn show_raw_logs() -> bool {
    get().style.show_raw_logs()
}

/// True when running inside a Docker/Podman container.
pub fn in_container() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup")
            .map(|s| s.contains("docker") || s.contains("containerd") || s.contains("libpod"))
            .unwrap_or(false)
}

pub struct Ux {
    pub style: OutputStyle,
}

impl Ux {
    //  Public colour helpers (for CLI output)

    pub fn green(&self, s: &str) -> String {
        Colour::green(s)
    }
    pub fn red(&self, s: &str) -> String {
        Colour::red(s)
    }
    pub fn yellow(&self, s: &str) -> String {
        Colour::yellow(s)
    }
    pub fn cyan(&self, s: &str) -> String {
        Colour::cyan(s)
    }
    pub fn magenta(&self, s: &str) -> String {
        Colour::magenta(s)
    }
    pub fn dim(&self, s: &str) -> String {
        Colour::dim(s)
    }
    pub fn bold(&self, s: &str) -> String {
        Colour::bold(s)
    }

    /// Pretty-print and colour a JSON string for terminal display.
    ///
    /// When `NO_COLOR` is set, returns the input unchanged (no ANSI
    /// escapes).  Otherwise applies:
    ///
    /// | token          | colour   |
    /// |----------------|----------|
    /// | keys           | cyan     |
    /// | string values  | green    |
    /// | numbers        | yellow   |
    /// | `true`/`false` | magenta  |
    /// | `null`         | dim red  |
    /// | punctuation    | default  |
    pub fn colorize_json(&self, json: &str) -> String {
        if !use_colour() {
            return json.to_string();
        }
        colorize_json_str(json)
    }

    //  Startup banner

    /// Print the startup block - banner, hints, and connection info.
    /// Called once from `run_server`.
    pub fn print_startup(
        &self,
        port: u16,
        version: &str,
        handshake: Option<&str>,
        tier2_configured: bool,
        tier3_configured: bool,
    ) {
        if !self.style.show_banner() {
            return;
        }

        let mut lines: Vec<String> = Vec::new();

        //  Identity
        lines.push(format!(
            "  #  {} {} - thumbnail server",
            Colour::bold("☂  Thumbrella"),
            Colour::dim(version),
        ));

        //  Release info (docker image tag, etc.)
        if let Ok(release) = std::fs::read_to_string("/etc/thumbrella-release") {
            let line = release.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                lines.push(format!("  # release: {line}"));
            }
        }

        //  Docs
        lines.push("  # docs: https://thumbrella.dev/docs/".to_string());

        //  Hints
        if self.style.show_hints()
            && !tier2_configured
            && !tier3_configured
            && !crate::check::has_builtin_renderer()
        {
            lines.push(format!(
                "  # hint: {} {} {}",
                "No higher tiers configured - only basic formats will render.",
                Colour::dim("Set"),
                Colour::bold("TBR_TIER2=http://tier2:8000"),
            ));
        }

        //  Container hint
        if in_container() {
            lines.push(format!(
                "  # hint: running in a container - map with -p HOST:{port} to access from the host",
            ));
        }

        //  Connection
        let connect = if let Some(hs) = handshake {
            let masked = Self::mask_handshake(hs);
            Colour::dim(&format!("TBR_CONNECT=http://localhost:{port},{masked} (use secret handshake)"))
        } else {
            Colour::dim(&format!("TBR_CONNECT=http://localhost:{port}"))
        };
        lines.push(format!("  # clients:  {connect}",));

        //  Startup tips - short extra lines queued by tier binaries (cloud nudges, ...)
        if self.style.show_hints() {
            for tip in drain_startup_tips() {
                lines.push(format!("  # {tip}"));
            }
        }

        //  Listening (always last)
        let listen_addr = if in_container() {
            let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "0.0.0.0".to_string());
            Colour::green(&format!("http://{host}:{port} (inside container)"))
        } else {
            Colour::green(&format!("http://0.0.0.0:{port}"))
        };
        lines.push(format!("  # listening on {listen_addr}"));

        for line in &lines {
            let _ = io::stdout().write_all(line.as_bytes());
            let _ = io::stdout().write_all(b"\n");
        }
        let _ = io::stdout().write_all(b"\n");
    }

    //  Helpers

    /// Mask a handshake value for display in the startup banner.
    ///
    /// Shows the first few characters followed by asterisks for the rest.
    /// - < 6 chars → all asterisks
    /// - < 12 chars → first 2 + `*` for remaining
    /// - >= 12 chars → first 4 + `*` for remaining
    pub fn mask_handshake(value: &str) -> String {
        let len = value.len();
        if len == 0 {
            return "***".to_string();
        }
        let (show, rest) = if len < 6 {
            (0, len)
        } else if len < 12 {
            (2, len - 2)
        } else {
            (4, len - 4)
        };
        let prefix = &value[..show.min(len)];
        let stars = "*".repeat(rest);
        format!("{prefix}{stars}")
    }

    /// Mask secrets in a connect string for display.
    ///
    /// Connect strings have the form `url,header...`.  Each segment is masked:
    /// - URL (first segment) — left visible
    /// - Auth tokens (`tbr_[a-z]_` prefix) — keep prefix, mask body
    /// - `key=value` headers — mask the value
    /// - Bare values — mask fully (handshake shorthand)
    pub fn mask_connect_string(value: &str) -> String {
        let segments: Vec<&str> = value.split(',').collect();
        if segments.is_empty() {
            return Self::mask_handshake(value);
        }

        // Bare token / no URL: the whole value is a single segment with
        // no comma — mask it as an auth token when recognised.
        if segments.len() == 1 && !segments[0].contains("://") {
            let seg = segments[0];
            if seg.as_bytes().len() >= 6
                && seg.as_bytes().starts_with(b"tbr_")
                && seg.as_bytes()[4].is_ascii_lowercase()
                && seg.as_bytes()[5] == b'_'
            {
                let prefix = &seg[..6];
                let rest = &seg[6..];
                return format!("{prefix}{}", Self::mask_handshake(rest));
            }
            return Self::mask_handshake(value);
        }

        let mut out = segments[0].to_string(); // URL — visible
        for seg in &segments[1..] {
            let masked = if let Some((k, v)) = seg.split_once('=') {
                format!("{k}={}", Self::mask_handshake(v))
            } else if seg.as_bytes().len() >= 6
                && seg.as_bytes().starts_with(b"tbr_")
                && seg.as_bytes()[4].is_ascii_lowercase()
                && seg.as_bytes()[5] == b'_'
            {
                // Auth token: keep tbr_x_ prefix, mask body
                let prefix = &seg[..6];
                let rest = &seg[6..];
                format!("{prefix}{}", Self::mask_handshake(rest))
            } else {
                Self::mask_handshake(seg)
            };
            out.push(',');
            out.push_str(&masked);
        }
        out
    }

    //  Request logging

    /// Log the start of a batch request.  Returns a token for per-item logging.
    pub fn log_batch_start(&self, method: &str, path: &str, count: usize, client_ip: Option<&str>) {
        let ip = match client_ip {
            Some(ip) => format!(" from {ip}"),
            None => String::new(),
        };
        let header = format!(
            "{method} {path}  {count} {items}{ip}\n",
            method = Colour::cyan(method),
            path = path,
            count = count,
            items = if count == 1 { "item" } else { "items" },
        );
        let _ = io::stdout().write_all(header.as_bytes());
    }

    /// Log a single thumbnail result.  Call for each item in a batch.
    #[allow(clippy::too_many_arguments)]
    pub fn log_thumb_result(
        &self,
        url: &str,
        status: u16,
        duration_ms: u64,
        kind: Option<&str>,
        extension: Option<&str>,
        _source: Option<&str>,
        message: Option<&str>,
    ) {
        let status_str = if (200..300).contains(&status) {
            Colour::green(&format!("{}", status))
        } else if (400..500).contains(&status) {
            Colour::yellow(&format!("{}", status))
        } else {
            Colour::red(&format!("{}", status))
        };

        let format_str = match (kind, extension) {
            (Some(k), Some(e)) if !e.is_empty() => format!("{k} {e}"),
            (Some(k), _) => k.to_string(),
            _ => String::new(),
        };

        let msg_str = match message {
            Some(m) if !m.is_empty() => format!(" - {m}"),
            _ => String::new(),
        };

        let line = format!(
            "  {status}  {duration:>5}ms  {format_str:<16}  {url}{msg_str}\n",
            status = status_str,
            duration = duration_ms,
            format_str = format_str,
            url = Colour::dim(url),
            msg_str = Colour::yellow(&msg_str),
        );
        let _ = io::stdout().write_all(line.as_bytes());
    }

    /// Log a single-thumb request (GET /thumb).
    #[allow(clippy::too_many_arguments)]
    pub fn log_single_thumb(
        &self,
        method: &str,
        path: &str,
        url: &str,
        status: u16,
        duration_ms: u64,
        kind: Option<&str>,
        extension: Option<&str>,
        source: Option<&str>,
        message: Option<&str>,
        client_ip: Option<&str>,
    ) {
        let ip = match client_ip {
            Some(ip) => format!(" from {ip}"),
            None => String::new(),
        };
        let header = format!("{method} {path}{ip}\n", method = Colour::cyan(method), path = path,);
        let _ = io::stdout().write_all(header.as_bytes());
        self.log_thumb_result(url, status, duration_ms, kind, extension, source, message);
    }

    //  Error messages

    /// Print a fatal startup error with a suggested fix.
    pub fn fatal(&self, problem: &str, suggestion: &str) -> ! {
        let msg = format!(
            "\n  {label} {problem}\n  {fix_label} {suggestion}\n",
            label = Colour::red("error:"),
            problem = problem,
            fix_label = Colour::dim("  fix:"),
            suggestion = suggestion,
        );
        let _ = io::stderr().write_all(msg.as_bytes());
        std::process::exit(1);
    }

    /// Print a non-fatal warning.
    pub fn warn(&self, problem: &str, suggestion: &str) {
        let msg = format!(
            "  {label} {problem}\n  {fix_label} {suggestion}\n",
            label = Colour::yellow("warn:"),
            problem = problem,
            fix_label = Colour::dim("  fix:"),
            suggestion = suggestion,
        );
        let _ = io::stderr().write_all(msg.as_bytes());
    }

    /// Print a single-line startup issue (used by `serve` after the banner).
    pub fn print_startup_issue(&self, msg: &str) {
        let line = format!("  {} {}\n", Colour::red("error:"), msg,);
        let _ = io::stdout().write_all(line.as_bytes());
    }
}

//  JSON colouriser

/// Colour a pretty-printed JSON string for terminal display.
///
/// Simple character-level state machine that tracks whether the cursor
/// is inside a string (and whether it's a key or value), a number, or
/// a bareword (`true`/`false`/`null`).
fn colorize_json_str(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 1024);
    let chars: Vec<char> = json.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        match ch {
            '"' => {
                // Determine whether this string is a key or a value.
                // A key is a string followed (after optional whitespace) by ':'.
                let start = i;
                i += 1; // skip opening quote
                while i < len && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    } // skip escaped char
                    i += 1;
                }
                i += 1; // skip closing quote

                let token: String = chars[start..i].iter().collect();

                // Peek ahead for ':' (key) vs ',' or '}' or end (value).
                let mut j = i;
                while j < len && (chars[j] == ' ' || chars[j] == '\n' || chars[j] == '\r' || chars[j] == '\t')
                {
                    j += 1;
                }
                if j < len && chars[j] == ':' {
                    out.push_str(&Colour::cyan(&token));
                } else {
                    out.push_str(&Colour::green(&token));
                }
            }

            '-' | '0'..='9' => {
                // Number - read until non-number char.
                let start = i;
                while i < len && matches!(chars[i], '-' | '+' | '0'..='9' | '.' | 'e' | 'E') {
                    i += 1;
                }
                let token: String = chars[start..i].iter().collect();
                out.push_str(&Colour::yellow(&token));
            }

            't' if json[i..].starts_with("true") => {
                out.push_str(&Colour::magenta("true"));
                i += 4;
            }

            'f' if json[i..].starts_with("false") => {
                out.push_str(&Colour::magenta("false"));
                i += 5;
            }

            'n' if json[i..].starts_with("null") => {
                out.push_str(&Colour::dim("null"));
                i += 4;
            }

            _ => {
                out.push(ch);
                i += 1;
            }
        }
    }

    out
}
