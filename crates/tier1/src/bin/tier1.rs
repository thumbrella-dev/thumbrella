//! Tier 1 native HTTP server entry point.
//!
//! Binds an axum server and serves the batch, thumb, and health endpoints.
//! Configuration is read from environment variables via `AppConfig::from_env`.

use axum::{Router, routing::{get, post}};
use std::net::SocketAddr;
use tier1::{config::AppConfig, routes};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    // Initialise tracing.  RUST_LOG controls the filter; default to info.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = AppConfig::from_env();
    tracing::info!(version = tier1::TBR_VERSION, "tier1 starting");

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/batch", post(routes::batch));

    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));
    tracing::info!(%addr, "listening");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
