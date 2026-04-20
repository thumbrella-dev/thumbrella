//! Dispatch hook for forwarding items to the next tier.
//!
//! Tier 1 calls `try_dispatch_tier2` whenever it cannot handle a source format.
//! The handler can be wired up in two ways:
//!
//! - **In-process** (combined binary): call `register_tier2` at startup with a
//!   closure that delegates to the Tier 2 pipeline.  This is the path used when
//!   the Tier 2 binary is the deployed service.
//!
//! - **Remote HTTP** (standalone Tier 1 binary): not yet implemented; currently
//!   returns `None` (unsupported) so callers propagate the original error.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use crate::{ItemRequest, ItemResult, ThumbnailProfile, ThumbnailRequestState};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub type Tier2Handler =
    Box<dyn Fn(ItemRequest, ThumbnailProfile, ThumbnailRequestState) -> BoxFuture<Option<ItemResult>> + Send + Sync>;

static TIER2: OnceLock<Tier2Handler> = OnceLock::new();

/// Register the in-process Tier 2 handler.
///
/// Call this once at binary startup, before serving any requests.
/// Subsequent calls are silently ignored (first registration wins).
pub fn register_tier2(handler: Tier2Handler) {
    let _ = TIER2.set(handler);
}

/// Try to process `item` via Tier 2.
///
/// Returns `Some(result)` if a handler is registered and it accepted the item.
/// Returns `None` when no handler is registered and no remote is configured,
/// meaning the caller should surface the original error to the client.
pub async fn try_dispatch_tier2(
    item: &ItemRequest,
    profile: &ThumbnailProfile,
    state: &ThumbnailRequestState,
) -> Option<ItemResult> {
    if let Some(handler) = TIER2.get() {
        let mut promoted = state.clone();
        promoted.increment_hop();
        return handler(item.clone(), profile.clone(), promoted).await;
    }

    // TODO: HTTP dispatch to a configured remote Tier 2 endpoint.
    None
}
