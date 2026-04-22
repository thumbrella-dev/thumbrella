//! Thumbnail request state tracked across tier promotions.
//!
//! This captures transport, sniffing, and sparse-buffer details in a single
//! object that can be passed in-process or serialized for remote promotion.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::{ItemRequest, SourceMetadata, SourceRef};

const DEFAULT_BLOCK_SIZE: usize = 8 * 1024;
const DEFAULT_MAX_CACHED_BYTES: usize = 512 * 1024;
const DEFAULT_HEAD_CACHE_BYTES: usize = 256 * 1024;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Sparse block cache that avoids allocating a monolithic source-sized buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparseFileBuffer {
    block_size: usize,
    max_cached_bytes: usize,
    cached_bytes: usize,
    streaming_passthrough: bool,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl Default for SparseFileBuffer {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            max_cached_bytes: DEFAULT_MAX_CACHED_BYTES,
            cached_bytes: 0,
            streaming_passthrough: false,
            blocks: BTreeMap::new(),
        }
    }
}

impl SparseFileBuffer {
    /// Cache a sparse byte range if we're still in metadata/small-window mode.
    pub fn cache_range(&mut self, offset: u64, bytes: &[u8]) {
        if self.streaming_passthrough || bytes.is_empty() {
            return;
        }

        let block_size = self.block_size as u64;
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let absolute = offset + cursor as u64;
            let block_index = absolute / block_size;
            let in_block = (absolute % block_size) as usize;
            let take = (self.block_size - in_block).min(bytes.len() - cursor);

            if !self.blocks.contains_key(&block_index) {
                if self.cached_bytes + self.block_size > self.max_cached_bytes {
                    self.streaming_passthrough = true;
                    return;
                }
                self.blocks.insert(block_index, vec![0u8; self.block_size]);
                self.cached_bytes += self.block_size;
            }

            if let Some(block) = self.blocks.get_mut(&block_index) {
                block[in_block..in_block + take].copy_from_slice(&bytes[cursor..cursor + take]);
            }

            cursor += take;
        }
    }

    pub fn set_streaming_passthrough(&mut self, enabled: bool) {
        self.streaming_passthrough = enabled;
    }

    pub fn is_streaming_passthrough(&self) -> bool {
        self.streaming_passthrough
    }
}

/// Request state object tracked as work moves from tier to tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbnailRequestState {
    pub request_id: String,
    pub item_id: Option<String>,
    /// Original URL as supplied by the caller. May contain expiring auth tokens.
    /// Request-lifetime only — never persist or log this value.
    pub source_url: String,
    /// Canonical URL: auth tokens stripped, scheme+host normalised. Resolved
    /// from the fetch response in `observe_prefix`; falls back to `source_url`
    /// until the first fetch completes. Safe to persist, log, and use as a
    /// cache key component.
    pub source_url_canonical: String,
    pub account_id: Option<String>,
    pub hop_count: u32,
    pub bytes_downloaded: u64,
    pub sniffed_mime: Option<String>,
    pub sniffed_format: Option<String>,
    pub source_meta: Option<SourceMetadata>,
    head_bytes: Vec<u8>,
    pub sparse_buffer: SparseFileBuffer,
}

impl ThumbnailRequestState {
    pub fn new(item: &ItemRequest) -> Self {
        let source_url = match &item.source {
            SourceRef::Url { url } => url.clone(),
        };
        let request_id = format!("req-{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed));

        Self {
            request_id,
            item_id: item.id.clone(),
            source_url_canonical: source_url.clone(),
            source_url,
            account_id: None,
            hop_count: 0,
            bytes_downloaded: 0,
            sniffed_mime: None,
            sniffed_format: None,
            source_meta: None,
            head_bytes: Vec::new(),
            sparse_buffer: SparseFileBuffer::default(),
        }
    }

    pub fn observe_prefix(&mut self, bytes: &[u8], meta: &SourceMetadata) {
        self.bytes_downloaded = self.bytes_downloaded.saturating_add(bytes.len() as u64);
        self.source_meta = Some(meta.clone());

        // Update the canonical URL once we have it from the fetch response.
        if let Some(canonical) = &meta.canonical_url {
            self.source_url_canonical = canonical.clone();
        }

        if self.sniffed_mime.is_none() {
            self.sniffed_mime = meta.magic_mime.clone().or_else(|| meta.content_type.clone());
        }

        self.sparse_buffer.cache_range(0, bytes);

        if self.head_bytes.len() < DEFAULT_HEAD_CACHE_BYTES {
            let remaining = DEFAULT_HEAD_CACHE_BYTES - self.head_bytes.len();
            let take = remaining.min(bytes.len());
            self.head_bytes.extend_from_slice(&bytes[..take]);
        }
    }

    pub fn note_stream_bytes(&mut self, bytes: usize) {
        self.bytes_downloaded = self.bytes_downloaded.saturating_add(bytes as u64);
    }

    /// Stop sparse caching and switch to pass-through streaming mode.
    pub fn switch_to_streaming_mode(&mut self) {
        self.sparse_buffer.set_streaming_passthrough(true);
    }

    pub fn head_bytes(&self) -> &[u8] {
        &self.head_bytes
    }

    pub fn increment_hop(&mut self) {
        self.hop_count = self.hop_count.saturating_add(1);
    }
}