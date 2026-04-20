//! Thumbrella Tier 2 server — the main HTTP entry point.
//!
//! All endpoints ultimately route through the batch pipeline. Simple
//! single-item facades are thin wrappers that construct a one-item batch.

use axum::{Router, routing::{get, post}};
use std::net::SocketAddr;
use thumbrella_tier1::routes;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/dev", get(routes::dev))
        .route("/batch", post(routes::batch));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
