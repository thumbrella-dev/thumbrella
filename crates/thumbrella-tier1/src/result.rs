//! Result and event types — the common output shape for all tiers.
//!
//! The pipeline emits a stream of `ItemEvent` values. Simple endpoints collect
//! them all; streaming endpoints forward each one as it arrives.

use serde::{Deserialize, Serialize};
use crate::source::SourceMetadata;
use crate::media::MediaMetadata;

/// High-level outcome of processing a single batch item.
///
/// Matches the `job_status` field described in schema.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Thumbnail generated successfully this request.
    Success,
    /// Result returned from cache — no reprocessing was needed.
    Cached,
    /// Source unchanged since the caller's supplied ETag.
    NotModified,
    /// Processing failed; see the `error` field for details.
    Failed,
    /// Request accepted but deferred to a higher-tier worker.
    Defer,
}

/// Per-HTTP-request tracking record.
///
/// One of these is created for each inbound HTTP request (batch call, dev page
/// load, etc.) and links together all the per-thumbnail `ServerInfo` records
/// produced by that request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub customer: String,
    pub host: String,
    pub path: String,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Wall-clock seconds from request start to all items complete.
    /// Populated after all pipeline tasks finish; None while in-flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
}

/// Per-thumbnail server-side tracking record.
///
/// Populated on every request regardless of developer mode. Whether it is
/// included in the public batch API response is controlled by
/// `THUMBRELLA_DEVELOPER_MODE`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    /// ID of the enclosing HTTP request (`RequestRecord.id`).
    pub request_id: String,
    /// Canonical fetch URL (query params / auth tokens stripped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_url: Option<String>,
    /// Hashed compound cache key: sha256("{customer}:{canonical_url}").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    /// Source byte length from Content-Length header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_size: Option<u64>,
    /// Source MIME type (magic-sniffed preferred over Content-Type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_mime: Option<String>,
    /// ETag / Last-Modified validator token as returned by upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_etag: Option<String>,
    /// Whether the upstream source supports byte-range requests.
    pub fetch_ranges: bool,
    /// Last-Modified header value from the upstream source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_last_modified: Option<String>,
    /// Total bytes received from the upstream source.
    pub download_bytes: u64,
    /// Extra bytes fetched in a tail read (e.g. TIFF range request).
    pub download_tail: u64,
    /// Seconds spent waiting for upstream download(s).
    pub download_duration: f64,
    /// Seconds spent on decode and pre-processing (excluding encode).
    pub render_duration: f64,
    /// Seconds spent on the image encode / post-process step.
    pub process_duration: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_art: Option<bool>,
    /// Byte length of the encoded JPEG thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_bytes: Option<u64>,
    pub server_host: String,
    pub server_tier: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemProperties {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl ItemProperties {
    pub fn from_dimensions(width: u32, height: u32) -> Self {
        Self {
            width: Some(width),
            height: Some(height),
        }
    }
}

// ServerInfo is defined above (replaces old struct)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiItemResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<JobStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_base64_bytes"
    )]
    pub thumbnail: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<crate::media::MediaType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<ItemProperties>,
    /// Byte length of the encoded JPEG thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

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
    /// Thumbnail generated (or retrieved from cache). Contains the JPEG bytes and media metadata.
    Thumbnail {
        id: Option<String>,
        /// Base64-encoded JPEG bytes. Will be replaced by a streaming blob ref
        /// once the streaming endpoint is live.
        #[serde(with = "base64_bytes")]
        jpeg: Vec<u8>,
        /// Decode strategy, dimensions, metrics, and warnings
        #[serde(default)]
        media: MediaMetadata,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub source_meta: Option<SourceMetadata>,
    /// Base64-encoded JPEG thumbnail, if one was produced.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_base64_bytes"
    )]
    pub thumbnail: Option<Vec<u8>>,
    /// Media metadata (strategy, metrics, properties, warnings)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaMetadata>,
    /// High-level media category (image, video, audio, document, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<crate::media::MediaType>,
    /// Canonical file extension without dot (jpeg, png, pdf, mp4, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    /// Processing outcome for this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_status: Option<JobStatus>,
    /// Wall time for the request handling path in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_duration: Option<f64>,
    /// Total source bytes read while handling this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_data: Option<u64>,
    /// Public-facing render strategy family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_strategy: Option<String>,
    /// Stable item properties exposed in the public response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<ItemProperties>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    pub error: Option<String>,
}

impl Default for ItemResult {
    fn default() -> Self {
        Self {
            id: None,
            url: None,
            source_meta: None,
            thumbnail: None,
            media: None,
            media_type: None,
            extension: None,
            job_status: None,
            job_duration: None,
            job_data: None,
            job_strategy: None,
            properties: None,
            server: None,
            error: None,
        }
    }
}

/// Response body for the synchronous batch endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResponse {
    pub request: RequestRecord,
    pub items: Vec<ApiItemResult>,
}

impl BatchResponse {
    pub fn from_item_results(items: Vec<ItemResult>, request: RequestRecord) -> Self {
        Self {
            request,
            items: items.into_iter().map(ItemResult::into_api).collect(),
        }
    }
}

impl ItemResult {
    pub fn into_api(self) -> ApiItemResult {
        let ItemResult {
            id: _,
            url,
            source_meta,
            thumbnail,
            media: _,
            media_type,
            extension,
            job_status,
            job_duration,
            job_data: _,
            job_strategy,
            properties,
            server,
            error,
        } = self;

        let etag = source_meta.as_ref().and_then(|meta| meta.etag.clone());
        let mime = source_meta.as_ref().and_then(|meta| {
            meta.magic_mime.clone().or_else(|| meta.content_type.clone())
        });
        let length = source_meta.as_ref().and_then(|meta| meta.content_length);
        let thumbnail_bytes = thumbnail.as_ref().map(|b| b.len() as u64);

        ApiItemResult {
            url,
            duration: job_duration,
            status: job_status,
            strategy: job_strategy,
            etag,
            thumbnail,
            mime,
            length,
            r#type: media_type,
            extension,
            properties,
            thumbnail_bytes,
            server,
            error,
        }
    }
}

pub fn public_job_strategy(raw_strategy: &str) -> String {
    let strategy = match raw_strategy {
        "progressive_partial" | "jpeg_progressive" | "png_interlaced_partial" | "png_interlaced" => "progressive",
        "embedded_jpeg_thumbnail" | "tier2_embedded_heic_thumbnail" | "odt_package_thumbnail" | "docx_package_thumbnail" | "container_internal" => "embedded",
        "full_image" | "tier2_libav_still" | "tier2_libav_video" | "tier2_libav_heic" | "tier2_libav_avif" | "tier2_libav_exr" => "render",
        _ => "fallback",
    };
    strategy.to_string()
}

// ---------------------------------------------------------------------------
// Base64 serde helpers
// ---------------------------------------------------------------------------

mod base64_bytes {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        let enc = STANDARD.encode(v);
        enc.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

mod option_base64_bytes {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
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
            Some(s) => STANDARD
                .decode(s)
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }

    use serde::Deserialize;
}
