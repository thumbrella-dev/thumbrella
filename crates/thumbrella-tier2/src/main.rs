//! Thumbrella Tier 2 server entry point.
//!
//! All requests enter through the Tier 1 route handlers.  Before the server
//! starts, we register the Tier 2 in-process handler so that Tier 1's pipeline
//! dispatches unsupported formats (HEIC, video, AVIF, EXR, etc.) here directly
//! instead of over HTTP.

use axum::{Router, routing::{get, post}};
use std::net::SocketAddr;
use thumbrella_tier1::routes;
use thumbrella_tier2::pipeline;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Register the in-process Tier 2 handler so Tier 1's dispatch hook calls
    // into this binary rather than trying an HTTP round-trip.
    thumbrella_tier1::dispatch::register_tier2(Box::new(|item, profile, state| {
        Box::pin(async move { pipeline::try_process_item(&item, &profile, &state).await })
    }));

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/batch", post(routes::batch));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8001));
    tracing::info!("tier2 listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
