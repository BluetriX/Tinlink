//! LiveKit token issuing for voice channels.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use tinlink_protocol::{Permissions, UserId};
use uuid::Uuid;

use crate::access::channel_access;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::livekit;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct VoiceTokenResponse {
    pub url: String,
    pub token: String,
    pub identity: String,
    pub room: String,
}

/// Issues a token for the voice room of a channel. The room name is the
/// channel id; the grant reflects SPEAK and SHARE_SCREEN in that channel.
pub async fn channel_token(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
) -> AppResult<Json<VoiceTokenResponse>> {
    let (_, perms) = channel_access(&state.db, channel_id, user.id).await?;
    let voice_enabled: bool =
        sqlx::query_scalar("SELECT voice_enabled FROM channels WHERE id = $1")
            .bind(channel_id)
            .fetch_one(&state.db)
            .await?;
    if !voice_enabled {
        return Err(AppError::bad_request("voice is disabled in this channel"));
    }
    if !perms.contains(Permissions::CONNECT) {
        return Err(AppError::Forbidden);
    }
    let display_name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    let identity = UserId::new(&user.name, &state.config.domain)
        .map_err(|e| anyhow::anyhow!("stored user name is invalid: {e}"))?
        .to_string();
    let room = channel_id.to_string();
    let token = livekit::issue_token(
        &state.config.livekit,
        &identity,
        &display_name,
        &room,
        perms,
    )
    .map_err(anyhow::Error::from)?;
    Ok(Json(VoiceTokenResponse {
        url: state.config.livekit.public_url.clone(),
        token,
        identity,
        room,
    }))
}

#[derive(Debug, Deserialize)]
pub struct DevTokenRequest {
    /// Local user name; becomes `name@domain`.
    pub name: String,
    pub room: String,
}

/// Issues a token without authentication. Only enabled with
/// `TINLINK_DEV_INSECURE_TOKENS=true`, used by the desktop prototype.
pub async fn dev_token(
    State(state): State<AppState>,
    Json(req): Json<DevTokenRequest>,
) -> AppResult<Json<VoiceTokenResponse>> {
    if !state.config.dev_insecure_tokens {
        return Err(AppError::Forbidden);
    }
    let identity = UserId::new(&req.name, &state.config.domain)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    let token = livekit::issue_token(
        &state.config.livekit,
        &identity.to_string(),
        identity.name(),
        &req.room,
        Permissions::DEFAULT_EVERYONE,
    )
    .map_err(anyhow::Error::from)?;
    Ok(Json(VoiceTokenResponse {
        url: state.config.livekit.public_url.clone(),
        token,
        identity: identity.to_string(),
        room: req.room,
    }))
}
