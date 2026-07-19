//! SSRF-safe URL validation.
//!
//! Rejects URLs targeting private, loopback, link-local, and reserved IP
//! ranges.  Used by the API edge (Cloudflare Worker) and available to any
//! consumer of the tier1 library.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Returns `true` when `raw` is a safe, publicly-reachable HTTP(S) URL.
///
/// Checks (in order):
/// - Length <= `max_len`
/// - Scheme is `http` or `https`
/// - Host is present
/// - Host is not a private / loopback / reserved address (SSRF mitigation)
///
/// When `allow_localhost` is `true`, localhost and single-label hostnames are
/// permitted (for development/testing only).
///
/// Parsing is delegated to the `url` crate (WHATWG URL Standard) so that
/// percent-encoding, IDN, IPv6 zone IDs, and other edge cases are handled
/// correctly rather than by hand.
pub fn is_safe_url(raw: &str, allow_localhost: bool, max_len: usize) -> bool {
    if raw.len() > max_len {
        return false;
    }

    let parsed = match url::Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return false,
    };

    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }

    let host = match parsed.host() {
        Some(h) => h,
        None => return false,
    };

    match host {
        url::Host::Domain(name) => {
            let h = name.to_ascii_lowercase();
            if h.is_empty() {
                return false;
            }
            if !allow_localhost && (h == "localhost" || !h.contains('.')) {
                return false;
            }
        }
        url::Host::Ipv4(addr) => {
            if !allow_localhost && !is_safe_ipv4(addr) {
                return false;
            }
        }
        url::Host::Ipv6(addr) => {
            if !allow_localhost && !is_safe_ipv6(addr) {
                return false;
            }
        }
    }

    true
}

/// Returns `true` if the host of `url` is permitted by the given allowlist.
///
/// Each entry in `allowed` is either:
/// - An exact hostname (e.g. `"cdn.acme.com"`) - matches case-insensitively.
/// - A `*.`-prefixed wildcard (e.g. `"*.acme.com"`) - matches any subdomain,
///   but NOT the bare parent domain itself.
///
/// An empty `allowed` slice permits all hosts (no restriction).
pub fn url_host_allowed(url: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();

    allowed.iter().any(|pattern| {
        if let Some(suffix) = pattern.strip_prefix("*.") {
            let suffix = suffix.to_ascii_lowercase();
            host.ends_with(&format!(".{suffix}"))
        } else {
            host == pattern.to_ascii_lowercase()
        }
    })
}

/// Returns `false` for any IPv4 range that must not be reachable from the
/// public internet (loopback, private, link-local, reserved, etc.).
fn is_safe_ipv4(addr: Ipv4Addr) -> bool {
    if addr.is_loopback()          // 127.0.0.0/8
        || addr.is_private()       // 10/8, 172.16/12, 192.168/16
        || addr.is_link_local()    // 169.254/16
        || addr.is_unspecified()   // 0.0.0.0
        || addr.is_broadcast()     // 255.255.255.255
        || addr.is_multicast()     // 224.0.0.0/4
        || addr.is_documentation()
    // 192.0.2/24, 198.51.100/24, 203.0.113/24
    {
        return false;
    }
    let octets = addr.octets();
    match (octets[0], octets[1]) {
        (100, 64..=127) => return false, // shared address space (RFC 6598)
        (192, 0) => return false,        // IETF protocol assignments (192.0.0/24)
        (198, 18..=19) => return false,  // benchmarking (RFC 2544)
        (240..=255, _) => return false,  // reserved (RFC 1112)
        _ => {}
    }
    true
}

/// Returns `false` for any IPv6 range that must not be reachable from the
/// public internet (loopback, ULA, link-local, multicast, unspecified, etc.).
fn is_safe_ipv6(addr: Ipv6Addr) -> bool {
    if addr.is_loopback()        // ::1
        || addr.is_unspecified() // ::
        || addr.is_multicast()
    // ff00::/8
    {
        return false;
    }
    match addr.segments()[0] {
        0xfc00..=0xfdff => return false, // ULA
        0xfe80..=0xfebf => return false, // link-local
        _ => {}
    }
    true
}
