//! Tinlink server: REST API, permission checks and LiveKit token issuing.

pub mod access;
mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod events;
mod gateway;
pub mod livekit;
pub mod state;

use axum::Router;
use axum::routing::get;
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

pub use state::AppState;

/// Embedded database migrations, applied at startup.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/gateway", get(gateway::upgrade))
        .nest("/api", api::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn connect_db(url: &str) -> anyhow::Result<PgPool> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}
