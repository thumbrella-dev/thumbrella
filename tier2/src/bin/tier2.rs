//! Tier 2 binary - starts tier 1's pipeline with the tier 2 renderer registered.

#[tokio::main]
async fn main() {
    tier1::check::mark_tier2_builtin();

    tier1::cli::run_with_hook(2, |rt| async move {
        let rt = tier1::with_renderer(rt, tier2::Tier2Renderer::shared());
        tier1::with_shortcut_limits(rt, tier1::ShortcutLimits::TIER2)
    })
    .await;
}
