//! Result and event types — the common output shape for all tiers.
//!
//! The pipeline emits a stream of `ItemEvent` values. Simple endpoints collect
//! them all; streaming endpoints forward each one as it arrives.

use serde::{Deserialize, Serialize};
use crate::source::SourceMetadata;

/// A single event emitted by the processing pipeline for one item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ItemEvent {
    /// Item accepted into the pipeline.
    Accepted { id: Option<String> },
    /// Source metadata has been resolved (HEAD / partial read complete).
    SourceResolved {
        id: Option<String>,
        meta: SourceMetadata,
    },
    /// Source is unchanged since the caller's supplied ETag — no thumbnail needed.
    NotModified { id: Option<String> },
    /// Thumbnail generated (or retrieved from cache). Contains the JPEG bytes.
    Thumbnail {
        id: Option<String>,
        /// Base64-encoded JPEG bytes. Will be replaced by a streaming blob ref
        /// once the streaming endpoint is live.
        #[serde(with = "base64_bytes")]
        jpeg: Vec<u8>,
    },
    /// This item could not be processed.
    Error {
        id: Option<String>,
        message: String,
    },
}

/// Collected result for one item — all events flattened for the sync endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemResult {
    pub id: Option<String>,
    pub source_meta: Option<SourceMetadata>,
    /// Base64-encoded JPEG thumbnail, if one was produced.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_base64_bytes"
    )]
    pub thumbnail: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// Response body for the synchronous batch endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    pub items: Vec<ItemResult>,
}

// ---------------------------------------------------------------------------
// Base64 serde helpers
// ---------------------------------------------------------------------------

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        let mut enc = String::new();
        // Inline base64 — swap for the `base64` crate once added to dependencies.
        base64_encode(v, &mut enc);
        enc.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64_decode(&s).map_err(serde::de::Error::custom)
    }

    fn base64_encode(input: &[u8], out: &mut String) {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i + 2 < input.len() {
            let b = ((input[i] as u32) << 16) | ((input[i+1] as u32) << 8) | (input[i+2] as u32);
            out.push(TABLE[((b >> 18) & 63) as usize] as char);
            out.push(TABLE[((b >> 12) & 63) as usize] as char);
            out.push(TABLE[((b >> 6)  & 63) as usize] as char);
            out.push(TABLE[(b & 63) as usize] as char);
            i += 3;
        }
        match input.len() - i {
            1 => {
                let b = (input[i] as u32) << 16;
                out.push(TABLE[((b >> 18) & 63) as usize] as char);
                out.push(TABLE[((b >> 12) & 63) as usize] as char);
                out.push_str("==");
            }
            2 => {
                let b = ((input[i] as u32) << 16) | ((input[i+1] as u32) << 8);
                out.push(TABLE[((b >> 18) & 63) as usize] as char);
                out.push(TABLE[((b >> 12) & 63) as usize] as char);
                out.push(TABLE[((b >> 6)  & 63) as usize] as char);
                out.push('=');
            }
            _ => {}
        }
    }

    fn base64_decode(_s: &str) -> Result<Vec<u8>, &'static str> {
        // Placeholder — replace with the `base64` crate once added to dependencies.
        Err("base64 decoding not yet implemented (add the base64 crate)")
    }
}

mod option_base64_bytes {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => super::base64_bytes::serialize(bytes, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(_) => Err(serde::de::Error::custom("base64 decoding not yet implemented")),
        }
    }

    use serde::Deserialize;
}
