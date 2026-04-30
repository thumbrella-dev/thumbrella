//! Higher-tier handoff — the data bundle forwarded to tier-2/3 renderers.
//!
//! [`ThumbHandoff`] is the serialisable projection of the three portable
//! sub-structs on [`ThumbCook`] that travel between tiers.  It is built by
//! [`ThumbCook::to_handoff`] on the sending side and consumed by
//! `ThumbCook::from_handoff` on the receiving tier to reconstruct the cook
//! state at the render entry point.
//!
//! What travels and why:
//! - [`InputSpec`]      — original caller inputs; receiver needs url/etag.
//! - [`MediaInfo`]      — sniffed type info; skips re-running connect+inspect.
//! - [`SourceIdentity`] — cache key; tier-1 stores the result after receipt.
//! - `first_page`       — head-start bytes; receiver parses without a new request.
//!
//! What does NOT travel:
//! - `runtime`       — each tier constructs its own.
//! - `http_buf`      — live resource; moved via [`ThumbCook::http_take_reader`]
//!                     on the in-process path, reconnected fresh on the
//!                     out-of-process (serialised) path.
//! - `render_image`  — not yet populated at handoff time.
//! - `tel_*`         — per-tier; each tier tracks its own timing.
//! - `out_*`         — receiver populates fresh.

use serde::{Deserialize, Serialize};
use crate::cook::{InputSpec, MediaInfo, SourceIdentity};

/// Serialisable bundle forwarded to a higher-tier renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThumbHandoff {
    pub input:  InputSpec,
    pub media:  MediaInfo,
    pub src:    SourceIdentity,

    /// First ~4 KiB of the remote file captured from the inspect page cache.
    ///
    /// Forwarded as a head start on header parsing.  `None` when no data was
    /// cached before the connection was closed (unusual).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_page: Option<Vec<u8>>,
}
