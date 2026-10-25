mod config;
mod livekit;

use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tinlink_protocol::{Permissions, UserId};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    if config.dev_insecure_tokens {
        tracing::warn!(
            "TINLINK_DEV_INSECURE_TOKENS is enabled: voice tokens are issued without authentication"
        );
    }

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(addr = %config.bind, domain = %config.domain, "tinlink server listening");

    axum::serve(listener, router(AppState { config }))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/voice/token", post(voice_token))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct VoiceTokenRequest {
    /// Local user name; becomes `name@domain`.
    name: String,
    room: String,
}

#[derive(Debug, Serialize)]
struct VoiceTokenResponse {
    url: String,
    token: String,
    identity: String,
}

/// Issues a LiveKit token. Until accounts exist this only works with
/// `TINLINK_DEV_INSECURE_TOKENS=true` and grants the default permissions.
async fn voice_token(
    State(state): State<AppState>,
    Json(req): Json<VoiceTokenRequest>,
) -> Result<Json<VoiceTokenResponse>, (StatusCode, String)> {
    if !state.config.dev_insecure_tokens {
        return Err((
            StatusCode::FORBIDDEN,
            "authentication is not implemented yet".into(),
        ));
    }
    let identity = UserId::new(&req.name, &state.config.domain)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let token = livekit::issue_token(
        &state.config.livekit,
        &identity.to_string(),
        identity.name(),
        &req.room,
        Permissions::DEFAULT_EVERYONE,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(VoiceTokenResponse {
        url: state.config.livekit.public_url.clone(),
        token,
        identity: identity.to_string(),
    }))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
