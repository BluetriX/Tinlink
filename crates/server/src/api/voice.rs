//! LiveKit token issuing for voice channels.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use tinlink_protocol::{Permissions, UserId};
use uuid::Uuid;

use crate::access::{Member, channel_access};
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::livekit;
use crate::livekit::RoomServiceError;
use crate::state::AppState;
use crate::voice_sync;

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
    let (member, _) = channel_access(&state.db, channel_id, user.id).await?;
    let perms = voice_sync::voice_permissions(&state, &member, channel_id).await?;
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

/// True if `actor` may moderate `target` (strictly higher role, owner wins).
fn outranks(actor: &Member, target: &Member) -> bool {
    !target.is_owner && actor.highest_position() > target.highest_position()
}

async fn user_name(state: &AppState, user_id: Uuid) -> AppResult<String> {
    Ok(sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?)
}

fn map_room_error(err: RoomServiceError) -> AppError {
    match err {
        RoomServiceError::NotFound => AppError::NotFound,
        RoomServiceError::NotConfigured => {
            AppError::bad_request("voice moderation is not configured on this server")
        }
        RoomServiceError::Other(e) => AppError::Internal(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct MuteRequest {
    pub muted: bool,
}

/// Server-side mute of a member in the whole group (like a TS3 server mute).
pub async fn set_mute(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, target_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<MuteRequest>,
) -> AppResult<axum::http::StatusCode> {
    let actor = Member::load(&state.db, group_id, user.id).await?;
    actor.require(Permissions::MUTE_MEMBERS)?;
    let target = Member::load(&state.db, group_id, target_id).await?;
    if target_id != user.id && !outranks(&actor, &target) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("UPDATE members SET voice_muted = $3 WHERE group_id = $1 AND user_id = $2")
        .bind(group_id)
        .bind(target_id)
        .bind(req.muted)
        .execute(&state.db)
        .await?;
    voice_sync::refresh_group(&state, group_id, Some(target_id)).await?;
    state.events.publish(crate::events::Event {
        kind: EventKind::VoiceMuteChanged,
        group_id,
        channel_id: None,
        user_id: Some(target_id),
        data: serde_json::json!({ "muted": req.muted }),
    });
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Disconnects a member from a channel's voice room.
pub async fn disconnect(
    State(state): State<AppState>,
    user: AuthUser,
    Path((channel_id, target_id)): Path<(Uuid, Uuid)>,
) -> AppResult<axum::http::StatusCode> {
    let (actor, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if target_id != user.id {
        if !perms.contains(Permissions::MOVE_MEMBERS) {
            return Err(AppError::Forbidden);
        }
        let target = Member::load(&state.db, actor.group_id, target_id).await?;
        if !outranks(&actor, &target) {
            return Err(AppError::Forbidden);
        }
    }
    let name = user_name(&state, target_id).await?;
    state
        .rooms
        .remove(
            &channel_id.to_string(),
            &voice_sync::identity(&state, &name),
        )
        .await
        .map_err(map_room_error)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub to: Uuid,
}

/// Moves a member to another voice channel: they are disconnected here and
/// receive a `voice_moved` event telling the client which room to join.
pub async fn move_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((channel_id, target_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<MoveRequest>,
) -> AppResult<axum::http::StatusCode> {
    let (actor, perms) = channel_access(&state.db, channel_id, user.id).await?;
    let (_, to_perms) = channel_access(&state.db, req.to, user.id).await?;
    if !perms.contains(Permissions::MOVE_MEMBERS) || !to_perms.contains(Permissions::MOVE_MEMBERS) {
        return Err(AppError::Forbidden);
    }
    let target = Member::load(&state.db, actor.group_id, target_id).await?;
    if target_id != user.id && !outranks(&actor, &target) {
        return Err(AppError::Forbidden);
    }
    if crate::access::channel_group(&state.db, req.to).await? != actor.group_id {
        return Err(AppError::bad_request("target channel is in another group"));
    }
    // The moved member must be allowed to join the target.
    let target_perms = target.channel_permissions(&state.db, req.to).await?;
    if !target_perms.contains(Permissions::VIEW_CHANNEL | Permissions::CONNECT) {
        return Err(AppError::bad_request(
            "the member cannot join the target channel",
        ));
    }
    let name = user_name(&state, target_id).await?;
    state
        .rooms
        .remove(
            &channel_id.to_string(),
            &voice_sync::identity(&state, &name),
        )
        .await
        .map_err(map_room_error)?;
    state.events.publish(crate::events::Event {
        kind: EventKind::VoiceMoved,
        group_id: actor.group_id,
        channel_id: None,
        user_id: Some(target_id),
        data: serde_json::json!({ "from": channel_id, "to": req.to }),
    });
    Ok(axum::http::StatusCode::NO_CONTENT)
}
