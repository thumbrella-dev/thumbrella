//! HTTP presentation of cached thumbnails, shared by native and Worker routes.

use crate::CacheHints;
use sha2::{Digest, Sha256};

pub const PIN_REDIRECT_CACHE_CONTROL: &str = "public, max-age=5";

#[derive(Debug, Clone)]
pub struct PinnedThumbnail {
    pub bytes: Vec<u8>,
    pub cache: String,
    pub until: u64,
}

pub struct PinHeaders {
    pub etag: String,
    pub last_modified: Option<String>,
    pub cache_control: String,
    pub not_modified: bool,
}

pub fn etag_matches<'a>(etag: &str, values: impl IntoIterator<Item = &'a str>) -> bool {
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    values.into_iter().any(|value| {
        let mut remaining = value.trim();
        if remaining == "*" {
            return true;
        }
        while !remaining.is_empty() {
            let tag = remaining.strip_prefix("W/").unwrap_or(remaining);
            let Some(quoted) = tag.strip_prefix('"') else {
                return false;
            };
            let Some(end) = quoted.find('"') else {
                return false;
            };
            let matches = &tag[..end + 2] == etag;
            remaining = quoted[end + 1..].trim_start();
            if !remaining.is_empty() {
                let Some(rest) = remaining.strip_prefix(',') else {
                    return false;
                };
                remaining = rest.trim_start();
            }
            if matches {
                return true;
            }
        }
        false
    })
}

impl PinnedThumbnail {
    pub fn headers(
        &self,
        now: u64,
        if_none_match: Option<&str>,
        if_modified_since: Option<&str>,
    ) -> Result<PinHeaders, String> {
        let hints = if self.cache.is_empty() {
            CacheHints {
                no_store: true,
                ..Default::default()
            }
        } else {
            CacheHints::decode(&self.cache).ok_or("invalid cached thumbnail cache hints")?
        };
        let etag = hints.etag.unwrap_or_else(|| {
            let hash = Sha256::digest(&self.bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("\"{hash}\"")
        });
        if !etag.bytes().all(|b| b >= 0x20 && b != 0x7f)
            || !etag.strip_prefix("W/").unwrap_or(&etag).starts_with('"')
            || !etag.ends_with('"')
        {
            return Err("invalid cached thumbnail ETag".into());
        }
        let modified = hints
            .last_modified
            .as_deref()
            .map(httpdate::parse_http_date)
            .transpose()
            .map_err(|_| "invalid cached thumbnail Last-Modified")?;
        let not_modified = if let Some(value) = if_none_match {
            etag_matches(&etag, [value])
        } else {
            modified
                .zip(if_modified_since.and_then(|value| httpdate::parse_http_date(value).ok()))
                .is_some_and(|(modified, since)| modified <= since)
        };
        let max_age = hints.expires_at.unwrap_or(0).min(self.until).saturating_sub(now);
        let cache_control = if hints.no_store {
            "no-store".into()
        } else if hints.no_cache {
            "no-cache".into()
        } else {
            let mut policy =
                format!("{}, max-age={max_age}", if hints.private { "private" } else { "public" });
            if max_age > 0 && hints.immutable {
                policy.push_str(", immutable");
            }
            // Never authorize a browser to reuse a pin beyond its own deadline.
            if let Some(stale) = hints.stale_while_revalidate.filter(|_| !hints.no_cache) {
                let stale = u64::from(stale).min(self.until.saturating_sub(now.saturating_add(max_age)));
                if stale > 0 {
                    policy.push_str(&format!(", stale-while-revalidate={stale}"));
                }
            }
            policy
        };
        Ok(PinHeaders {
            etag,
            last_modified: hints.last_modified,
            cache_control,
            not_modified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thumbnail(hints: CacheHints) -> PinnedThumbnail {
        PinnedThumbnail {
            bytes: vec![1, 2, 3],
            cache: hints.encode(60),
            until: 2000,
        }
    }

    #[test]
    fn pin_headers_preserve_hints_with_validator_precedence_and_deadline_caps() {
        let pin = thumbnail(CacheHints {
            expires_at: Some(3000),
            etag: Some("\"source-v1\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            ..Default::default()
        });
        let headers = pin.headers(1000, Some("W/\"source-v1\""), None).unwrap();
        assert!(headers.not_modified);
        assert_eq!(headers.etag, "\"source-v1\"");
        assert_eq!(headers.cache_control, "public, max-age=1000");
        assert!(
            pin.headers(1000, None, Some("Wed, 21 Oct 2015 07:28:00 GMT"))
                .unwrap()
                .not_modified
        );
        assert!(
            !pin.headers(1000, None, Some("Tue, 20 Oct 2015 07:28:00 GMT"))
                .unwrap()
                .not_modified
        );
        assert!(
            !pin.headers(1000, Some("\"other\""), Some("Wed, 21 Oct 2015 07:28:00 GMT"))
                .unwrap()
                .not_modified
        );
        assert!(!pin.headers(1000, None, Some("invalid date")).unwrap().not_modified);
    }

    #[test]
    fn pin_headers_handle_stale_uncacheable_and_validator_free_entries() {
        let stale = thumbnail(CacheHints {
            expires_at: Some(1),
            ..Default::default()
        });
        assert_eq!(stale.headers(1000, None, None).unwrap().cache_control, "public, max-age=0");
        let headers = stale.headers(1000, None, None).unwrap();
        assert_eq!(
            headers.etag,
            "\"039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81\""
        );
        assert!(stale.headers(1000, Some(&headers.etag), None).unwrap().not_modified);
        let uncacheable = thumbnail(CacheHints {
            no_store: true,
            ..Default::default()
        });
        assert_eq!(uncacheable.headers(1000, None, None).unwrap().cache_control, "no-store");
        let revalidate = thumbnail(CacheHints {
            no_cache: true,
            etag: Some("\"A\"".into()),
            ..Default::default()
        });
        assert_eq!(revalidate.headers(1000, None, None).unwrap().cache_control, "no-cache");
    }
}
