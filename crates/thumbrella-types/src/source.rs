//! Source reference and transport metadata.

use serde::{Deserialize, Serialize};

/// A reference to an input source. Currently URL-only; uploads and storage
/// references will be added later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceRef {
    Url { url: String },
}

/// HTTP-level metadata learned about a source before or during download.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMetadata {
    /// MIME type from the Content-Type response header.
    pub content_type: Option<String>,
    /// MIME type from libmagic / infer byte sniffing.
    pub magic_mime: Option<String>,
    /// Byte length from Content-Length (may be absent or wrong for chunked).
    pub content_length: Option<u64>,
    /// ETag from the response, if present.
    pub etag: Option<String>,
    /// Last-Modified from the response, if present.
    pub last_modified: Option<String>,
    /// Whether the server indicated Accept-Ranges: bytes support.
    pub accepts_ranges: bool,
}