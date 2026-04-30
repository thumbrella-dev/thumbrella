//! Tier 1 binary — thin stub that delegates to [`tier1::cli::run`].

#[tokio::main]
async fn main() {
    tier1::cli::run().await;
}
