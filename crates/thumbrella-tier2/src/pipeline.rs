//! Tier 2 source pipeline.
//!
//! This layer is where Tier 2-specific source identification and decode paths
//! live. Unknown formats fall back to Tier 1 so Tier 2 remains a superset.

use thumbrella_tier1::{ItemRequest, ItemResult, SourceMetadata, SourceRef, ThumbnailProfile};

pub type RenderInfo = thumbrella_tier1::pipeline::RenderInfo;

/// Build source metadata for a local byte source.
pub fn metadata_from_local_bytes(bytes: &[u8], content_length: Option<u64>, last_modified: Option<String>) -> SourceMetadata {
    thumbrella_tier1::pipeline::metadata_from_local_bytes(bytes, content_length, last_modified)
}

/// Render a thumbnail from source bytes.
///
/// Tier 2-specific render loaders will be inserted ahead of the Tier 1 path.
pub fn render_thumbnail_from_bytes(bytes: &[u8], profile: &ThumbnailProfile) -> Result<(Vec<u8>, RenderInfo), String> {
    if let Some(result) = try_render_tier2(bytes, profile) {
        return result;
    }

    thumbrella_tier1::pipeline::render_thumbnail_from_bytes(bytes, profile)
}

/// Process one item with Tier 2 handlers first and Tier 1 fallback.
pub async fn process_item(item: &ItemRequest, profile: &ThumbnailProfile) -> ItemResult {
    if let Some(result) = try_process_item_tier2(item, profile).await {
        return result;
    }

    thumbrella_tier1::pipeline::process_item(item, profile).await
}

fn try_render_tier2(_bytes: &[u8], _profile: &ThumbnailProfile) -> Option<Result<(Vec<u8>, RenderInfo), String>> {
    // Placeholder for Tier 2 loaders (e.g. libav-backed decode selection).
    None
}

async fn try_process_item_tier2(item: &ItemRequest, _profile: &ThumbnailProfile) -> Option<ItemResult> {
    // Placeholder for Tier 2 remote/source handlers. This keeps an explicit
    // hook where Tier 2 can intercept supported source families first.
    let _url = source_url(&item.source)?;
    None
}

fn source_url(source: &SourceRef) -> Option<&str> {
    match source {
        SourceRef::Url { url } => Some(url.as_str()),
    }
}
