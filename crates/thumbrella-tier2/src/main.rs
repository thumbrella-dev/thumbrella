//! Thumbrella Tier 2 server entry point.

use axum::{Router, routing::{get, post}};
use std::net::SocketAddr;
use thumbrella_tier2::routes;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/batch", post(routes::batch));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8001));
    tracing::info!("tier2 listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
