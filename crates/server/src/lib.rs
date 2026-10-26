//! Tinlink server: REST API, permission checks and LiveKit token issuing.

pub mod access;
mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod events;
pub mod files;
mod gateway;
pub mod livekit;
pub mod presence;
mod ratelimit;
pub mod state;
pub mod tasks;
pub mod voice_sync;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, header};
use axum::routing::get;
use sqlx::PgPool;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

pub use state::AppState;

/// Embedded database migrations, applied at startup.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Content security policy of the web client: only its own scripts and
/// styles; media and signaling may go to LiveKit on another host.
const WEB_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' blob: data:; media-src 'self' blob:; connect-src 'self' ws: wss: https:; \
    object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/gateway", get(gateway::upgrade))
        .nest("/api", api::router());
    if let Some(dir) = state.config.web_dir.as_ref().filter(|d| d.is_dir()) {
        let web = Router::new()
            .fallback_service(ServeDir::new(dir))
            .layer(SetResponseHeaderLayer::overriding(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(WEB_CSP),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                HeaderName::from_static("permissions-policy"),
                HeaderValue::from_static("microphone=(self), display-capture=(self)"),
            ));
        router = router.merge(web);
    }
    router.layer(TraceLayer::new_for_http()).with_state(state)
}

pub async fn connect_db(url: &str) -> anyhow::Result<PgPool> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}
