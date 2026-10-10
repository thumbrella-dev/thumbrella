//! Opaque pin identifiers and backend-owned thumbnail aliases.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::CacheStore;
use crate::media::FileKind;
use crate::result::{ResultStatus, ThumbResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinClaim {
    Claimed,
    Conflict,
    Missing,
}

pub fn kind_letter(kind: FileKind) -> char {
    match kind {
        FileKind::Image => 'i',
        FileKind::Video => 'v',
        FileKind::Audio => 'a',
        FileKind::Vector => 's',
        FileKind::Document => 'd',
        FileKind::Geometry => 'g',
        FileKind::Archive => 'r',
        FileKind::Text => 't',
        FileKind::Binary => 'b',
        FileKind::Unknown => 'u',
    }
}

pub fn pin_kind(id: &str) -> FileKind {
    match id.as_bytes().first() {
        Some(b'i') => FileKind::Image,
        Some(b'v') => FileKind::Video,
        Some(b'a') => FileKind::Audio,
        Some(b's') => FileKind::Vector,
        Some(b'd') => FileKind::Document,
        Some(b'g') => FileKind::Geometry,
        Some(b'r') => FileKind::Archive,
        Some(b't') => FileKind::Text,
        Some(b'b') => FileKind::Binary,
        _ => FileKind::Unknown,
    }
}

pub fn valid_pin(id: &str) -> bool {
    id.len() == 13
        && matches!(
            id.as_bytes()[0],
            b'i' | b'v' | b'a' | b's' | b'd' | b'g' | b'r' | b't' | b'b' | b'u'
        )
        && id.as_bytes()[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Clone)]
pub(super) struct PinSecret([u8; 32]);

impl PinSecret {
    pub(super) fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    pub(super) fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| "stored pin secret must contain exactly 32 bytes".into())
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(super) fn candidate(&self, kind: FileKind, key: &str, attempt: u32) -> String {
        let mut hash = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts any key length");
        hash.update(b"thumbrella-pin-v1\0");
        hash.update(&crate::TBR_CACHE_VERSION.to_be_bytes());
        hash.update(&[kind_letter(kind) as u8]);
        hash.update(&(key.len() as u64).to_be_bytes());
        hash.update(key.as_bytes());
        hash.update(&attempt.to_be_bytes());
        let encoded = URL_SAFE_NO_PAD.encode(hash.finalize().into_bytes());
        format!("{}{}", kind_letter(kind), &encoded[..12])
    }
}

impl CacheStore {
    /// Issue or refresh a pin after real-thumbnail cache writes have completed.
    /// Successful placeholders derive an untracked candidate without accessing
    /// cache entries. Client-only freshness requires an existing server entry.
    pub async fn pin_result(
        &self,
        key: &str,
        result: &ThumbResult,
        ttl: u64,
    ) -> Result<Option<String>, String> {
        if ttl == 0 || result.status != ResultStatus::Success || result.media.is_none() {
            return Ok(None);
        }
        let Some(backend) = self.backend.as_ref() else {
            return Ok(None);
        };
        if let Some(media) = result.media.as_ref().filter(|media| !media.placeholder.is_empty()) {
            let id = backend.untracked_pin(media.kind, key).await?;
            return Ok(Some(format!("pin/{id}.jpeg")));
        }
        let entry = match backend.get_entry(key, 0).await {
            Some(entry) => Some(entry),
            None => backend.get_pin_entry(key).await,
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        if entry.media.thumbnail.is_empty() || !entry.media.placeholder.is_empty() {
            return Ok(None);
        }
        let id = match backend.issue_pin(entry.media.kind, key, ttl).await? {
            Some(id) => id,
            None => {
                // A chain's cold alias owner may have evicted its copy while a
                // hotter tier still has the representation we can safely pin.
                let mut copy = entry.clone();
                copy.pin_until = copy.pin_until.max(super::unix_now_secs().saturating_add(ttl));
                backend.promote(key.to_string(), copy).await;
                backend
                    .issue_pin(entry.media.kind, key, ttl)
                    .await?
                    .ok_or("pin entry disappeared while publishing its alias")?
            }
        };
        Ok(Some(format!("pin/{id}.jpeg")))
    }

    /// Return JPEG bytes only, without refreshing either retention deadline.
    pub async fn pin_thumbnail(&self, id: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(backend) = self.backend.as_ref() else {
            return Ok(None);
        };
        backend.pin_thumbnail(id).await
    }

    pub async fn pin_data(&self, id: &str) -> Result<Option<crate::http_cache::PinnedThumbnail>, String> {
        let Some(backend) = self.backend.as_ref() else { return Ok(None) };
        backend.pin_data(id).await
    }
}
