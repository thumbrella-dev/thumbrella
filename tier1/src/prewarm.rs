//! Speculative handoff wakeups - a fire-and-forget `/health` ping.
//!
//! # Why
//!
//! A higher-tier handoff server (tier 2 on Modal, tier 3 on a container host)
//! can be cold when a request arrives: the container has to boot, start the
//! thumbrella binary, and link its decoders before `/handoff` can be answered.
//! The cook already spends tens of milliseconds on the origin connection and
//! the type sniff before it knows whether a handoff is even needed, so a
//! `GET /health` issued at the start of that window gives a cold server a
//! useful head start at zero cost to the request.
//!
//! The response is never inspected and nothing waits for it.  It is fine for
//! the wakeup to be wasted: the URL may 404, inspection may sniff a different
//! format than the extension implied, or the shortcut handler may finish the
//! thumbnail without any handoff at all.
//!
//! # When
//!
//! [`ThumbCook::plan_handoff_wakes`](crate::cook::ThumbCook::plan_handoff_wakes)
//! is called after the cache phase resolved to a miss and before
//! `connect`/`inspect`, so:
//!
//! - a fresh cache or debounce hit never wakes anything,
//! - a request rejected or blocked earlier in intake never wakes anything,
//! - a run that cannot hand off (`render_disabled`) never wakes anything.
//!
//! # Cost control
//!
//! Each handoff server is pinged at most once per
//! [`WAKE_MIN_INTERVAL_SECS`].  The process-local record lives in
//! [`HandoffWake`] (on [`Runtime`]); a server that was
//! contacted by a real handoff counts too.  Embedders that run short-lived
//! isolates need a durable record as well - the Cloudflare worker keeps one in
//! the Workers cache API (see `cloud/src/cook.rs`).
//!
//! # Build note
//!
//! This module must compile on `wasm32-unknown-unknown`.  Planning is
//! platform-independent; only the default transport (`spawn`) is native.  A
//! wasm embedder calls [`HandoffWake`] and [`plans_for`] through the cook and
//! schedules the request itself.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cook::Runtime;
use crate::dispatch;
use crate::media::FileKind;

/// Minimum time between two wakeups aimed at the same handoff server.
///
/// A handoff server that answered any request in the last few minutes is warm;
/// waking it again only adds noise.  Five minutes comfortably covers a
/// container's idle-eviction horizon on the hosts we use.
pub const WAKE_MIN_INTERVAL_SECS: u64 = 300;

//  WakePlan

/// One planned wakeup: which handoff server to hit and with what headers.
#[derive(Debug, Clone)]
pub struct WakePlan {
    /// Tier that owns the target (2 or 3).  Used for logging and the
    /// process-local contact record.
    pub tier: u8,
    /// Handoff server base URL, exactly as configured in the connect target.
    pub url: String,
    /// Headers from the connect target (`x-tbr-handshake`, `Authorization`, …).
    /// `/health` sits behind the handshake middleware, so these are required.
    pub headers: HashMap<String, String>,
}

impl WakePlan {
    /// `GET <url>/health` - the endpoint every thumbrella server exposes.
    pub fn endpoint(&self) -> String {
        format!("{}/health", self.url.trim_end_matches('/'))
    }
}

//  HandoffWake

/// Process-local record of when each handoff server was last contacted.
///
/// Shared by every cook in the process through [`Runtime`], so concurrent items
/// in one batch collapse into a single wakeup.  Cloning shares the same atomics.
#[derive(Clone, Default)]
pub struct HandoffWake {
    tier2: Arc<AtomicU64>,
    tier3: Arc<AtomicU64>,
}

impl HandoffWake {
    /// Empty record - nothing contacted.
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, tier: u8) -> Option<&AtomicU64> {
        match tier {
            2 => Some(&self.tier2),
            3 => Some(&self.tier3),
            _ => None,
        }
    }

    /// Record that a request was just sent to the tier's handoff server.
    ///
    /// Called for real handoff requests as well as wakeups.
    pub fn record_contact(&self, tier: u8) {
        if let Some(slot) = self.slot(tier) {
            slot.store(now_secs(), Ordering::Relaxed);
        }
    }

    /// `true` when the tier's handoff server was contacted recently enough
    /// that another wakeup would be redundant.
    pub fn contacted_recently(&self, tier: u8) -> bool {
        let Some(slot) = self.slot(tier) else { return false };
        let last = slot.load(Ordering::Relaxed);
        last != 0 && now_secs().saturating_sub(last) < WAKE_MIN_INTERVAL_SECS
    }

    /// Claim the wakeup slot for `tier`.
    ///
    /// Returns `true` once per [`WAKE_MIN_INTERVAL_SECS`] window; the claim is
    /// recorded immediately so concurrent cooks in the same batch see only one
    /// winner.  Returns `false` for tiers this record does not track.
    pub fn claim(&self, tier: u8) -> bool {
        let Some(slot) = self.slot(tier) else { return false };
        let now = now_secs();
        let last = slot.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < WAKE_MIN_INTERVAL_SECS {
            return false;
        }
        slot.store(now, Ordering::Relaxed);
        true
    }
}

fn now_secs() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

//  Planning

/// Plan the wakeups a URL could justify, before the file is sniffed.
///
/// The extension-only preview stands in for the routing decision that
/// `inspect` will make later, so the plan mirrors the handoff chain in
/// [`crate::cook::ThumbCook::run_with_progress`]:
///
/// - **Tier 2** is woken whenever the chain can start at tier 2 or below,
///   which includes tier-1 image formats (the chain escalates those to tier 2
///   when the shortcut and render cannot finish them) and tier-3 formats when
///   no tier 3 is configured.
/// - **Tier 3** is woken only for extensions that route to tier 3 outright.
///   The chain can escalate a failed tier-2 handoff to tier 3 as well, but that
///   is a failure path and tier-3 renderers are expensive to boot, so it is not
///   predicted here.
///
/// A URL whose extension is not in the format manifest yields no plans: an
/// unknown extension is no evidence that a handoff will be needed.
pub fn plans_for(url: &str, runtime: &Runtime) -> Vec<WakePlan> {
    let Some(extension) = crate::pipeline::extension_from_url(url) else {
        return Vec::new();
    };
    let Some(preview) = dispatch::preview_route(&extension) else {
        return Vec::new();
    };

    let tier2 = &runtime.handoff_tier2;
    let tier3 = &runtime.handoff_tier3;

    // Same entry-tier adjustment the handoff chain applies.
    let start = if preview.tier == 1
        && preview.kind == FileKind::Image
        && tier2.url.is_some()
    {
        2
    } else if preview.tier == 3 && tier3.url.is_none() && tier2.url.is_some() {
        2
    } else {
        preview.tier
    };

    let mut plans = Vec::with_capacity(2);
    if start <= 2
        && let Some(base) = tier2.url.as_deref()
    {
        plans.push(WakePlan { tier: 2, url: base.to_string(), headers: tier2.headers.clone() });
    }
    // `exr` is excluded the same way the handoff chain excludes it: it has no
    // tier-3 renderer, so waking tier 3 for it would always be wasted.
    if preview.tier >= 3
        && extension != "exr"
        && let Some(base) = tier3.url.as_deref()
    {
        plans.push(WakePlan { tier: 3, url: base.to_string(), headers: tier3.headers.clone() });
    }
    plans
}

//  Native transport

/// Fire a wakeup on the native runtime, ignoring the outcome.
#[cfg(feature = "native")]
pub fn spawn(plan: WakePlan) {
    tokio::spawn(async move { ping(plan).await });
}

#[cfg(feature = "native")]
async fn ping(plan: WakePlan) {
    let Some(client) = client() else { return };
    let mut request = client.get(plan.endpoint());
    for (name, value) in &plan.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    match request.send().await {
        Ok(response) => tracing::debug!(
            target: "tier1::prewarm",
            tier = plan.tier,
            status = response.status().as_u16(),
            "handoff wakeup answered"
        ),
        Err(error) => tracing::debug!(
            target: "tier1::prewarm",
            tier = plan.tier,
            %error,
            "handoff wakeup failed"
        ),
    }
}

/// Shared reqwest client for wakeups.
///
/// Deliberately separate from the handoff transport: the wakeup must never
/// occupy a pooled connection or a connection slot that a real handoff is
/// about to need, and its timeouts are far shorter (a cold server may take
/// seconds to answer `/health`, and we do not care).
#[cfg(feature = "native")]
fn client() -> Option<&'static reqwest::Client> {
    use std::sync::OnceLock;
    use std::time::Duration;

    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .read_timeout(Duration::from_secs(3))
                .tcp_nodelay(true)
                .build()
                .ok()
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::ConnectTarget;

    fn runtime(tier2: Option<&str>, tier3: Option<&str>) -> Runtime {
        let mut runtime = (*Runtime::new(
            crate::cache::CacheStore::none(),
            crate::tracelog::TraceStore::new(Vec::new()),
            None,
            None,
            ConnectTarget::default(),
            ConnectTarget::default(),
            None,
            false,
            300,
            100,
            5,
            604_800,
            300,
        ))
        .clone();
        if let Some(url) = tier2 {
            runtime.handoff_tier2.url = Some(url.to_string());
        }
        if let Some(url) = tier3 {
            runtime.handoff_tier3.url = Some(url.to_string());
        }
        runtime
    }

    fn tiers(plans: &[WakePlan]) -> Vec<u8> {
        plans.iter().map(|plan| plan.tier).collect()
    }

    #[test]
    fn tier_one_image_wakes_only_tier_two() {
        // A tier-1 image can be escalated to tier 2, but tier 3 is a
        // failure path and is not predicted.
        let rt = runtime(Some("http://t2"), Some("http://t3"));
        let plans = plans_for("https://example.com/a.png", &rt);
        assert_eq!(tiers(&plans), vec![2]);
        assert_eq!(plans[0].endpoint(), "http://t2/health");
    }

    #[test]
    fn tier_two_format_wakes_only_tier_two() {
        let rt = runtime(Some("http://t2"), Some("http://t3"));
        assert_eq!(tiers(&plans_for("https://example.com/a.jpg", &rt)), vec![2]);
    }

    #[test]
    fn tier_three_format_wakes_only_tier_three() {
        let rt = runtime(Some("http://t2"), Some("http://t3"));
        assert_eq!(tiers(&plans_for("https://example.com/a.pdf", &rt)), vec![3]);
        // With no tier 3 configured the same extension starts at tier 2.
        let rt = runtime(Some("http://t2"), None);
        assert_eq!(tiers(&plans_for("https://example.com/a.pdf", &rt)), vec![2]);
    }

    #[test]
    fn unconfigured_servers_and_unknown_extensions_plan_nothing() {
        let rt = runtime(None, None);
        assert!(plans_for("https://example.com/a.png", &rt).is_empty());
        let rt = runtime(Some("http://t2"), Some("http://t3"));
        assert!(plans_for("https://example.com/a.txt", &rt).is_empty());
        assert!(plans_for("https://example.com/no-extension", &rt).is_empty());
    }

    #[test]
    fn exr_never_plans_a_tier_three_wakeup() {
        let rt = runtime(Some("http://t2"), Some("http://t3"));
        assert_eq!(tiers(&plans_for("https://example.com/a.exr", &rt)), vec![2]);
    }
    #[test]
    fn contact_claims_are_shared_and_expire_on_the_interval() {
        let wake = HandoffWake::new();
        assert!(wake.claim(2), "first wakeup is due");
        assert!(!wake.claim(2), "second wakeup within the window is not");
        assert!(wake.claim(3), "tiers are tracked separately");
        assert!(wake.contacted_recently(2));

        // Backdate the tier-2 record past the window.
        wake.tier2.store(now_secs().saturating_sub(WAKE_MIN_INTERVAL_SECS + 1), Ordering::Relaxed);
        assert!(!wake.contacted_recently(2));
        assert!(wake.claim(2));

        let clone = wake.clone();
        assert!(clone.contacted_recently(2), "clones share the record");
        assert!(!wake.claim(9), "unsupported tiers are never claimed");
    }

    #[cfg(feature = "native")]
    #[test]
    fn cooks_plan_one_wakeup_per_window_and_none_when_blocked() {
        use std::sync::Arc;

        let shared = Arc::new(runtime(Some("http://t2"), Some("http://t3")));
        let cook = crate::cook::ThumbCook::<crate::http_buf::PlatformStream>::from_input(
            crate::cook::InputSpec::new("https://example.com/a.png"),
            shared,
        );
        assert_eq!(tiers(&cook.plan_handoff_wakes()), vec![2]);
        assert!(cook.plan_handoff_wakes().is_empty(), "window is claimed by the first plan");

        // A blocked cook (over quota / token disabled) never wakes anything.
        let shared = Arc::new(runtime(Some("http://t2"), Some("http://t3")));
        let mut blocked = crate::cook::ThumbCook::<crate::http_buf::PlatformStream>::from_input(
            crate::cook::InputSpec::new("https://example.com/a.png"),
            shared,
        );
        blocked.render_disabled = true;
        assert!(blocked.plan_handoff_wakes().is_empty());
    }
}
