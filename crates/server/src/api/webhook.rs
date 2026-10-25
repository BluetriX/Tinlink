//! LiveKit webhook receiver: tracks who is in which voice channel and
//! removes temporary channels once their room is empty.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use serde::Deserialize;
use serde_json::json;
use tinlink_protocol::UserId;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::livekit;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct WebhookEvent {
    event: String,
    room: Option<Room>,
    participant: Option<Participant>,
}

#[derive(Debug, Deserialize)]
struct Room {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Participant {
    identity: String,
}

pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<StatusCode> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    livekit::verify_webhook(&state.config.livekit, auth, &body).map_err(|err| {
        tracing::warn!(error = %err, "rejected LiveKit webhook");
        AppError::Unauthorized
    })?;
    let event: WebhookEvent =
        serde_json::from_slice(&body).map_err(|e| AppError::bad_request(e.to_string()))?;

    // Rooms not named after a channel (e.g. the dev endpoint's) are ignored.
    let Some(channel_id) = event
        .room
        .as_ref()
        .and_then(|r| r.name.parse::<Uuid>().ok())
    else {
        return Ok(StatusCode::OK);
    };
    let Some(group_id): Option<Uuid> =
        sqlx::query_scalar("SELECT group_id FROM channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(&state.db)
            .await?
    else {
        return Ok(StatusCode::OK);
    };

    match event.event.as_str() {
        "participant_joined" | "participant_left" => {
            let Some(user_id) = local_user(&state, event.participant.as_ref()).await? else {
                return Ok(StatusCode::OK);
            };
            if event.event == "participant_joined" {
                sqlx::query("INSERT INTO voice_participants (channel_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                    .bind(channel_id)
                    .bind(user_id)
                    .execute(&state.db)
                    .await?;
                state.events.channel(
                    EventKind::VoiceJoined,
                    group_id,
                    channel_id,
                    json!({ "user_id": user_id }),
                );
            } else {
                sqlx::query(
                    "DELETE FROM voice_participants WHERE channel_id = $1 AND user_id = $2",
                )
                .bind(channel_id)
                .bind(user_id)
                .execute(&state.db)
                .await?;
                state.events.channel(
                    EventKind::VoiceLeft,
                    group_id,
                    channel_id,
                    json!({ "user_id": user_id }),
                );
            }
        }
        "room_finished" => {
            sqlx::query("DELETE FROM voice_participants WHERE channel_id = $1")
                .bind(channel_id)
                .execute(&state.db)
                .await?;
            let deleted = sqlx::query("DELETE FROM channels WHERE id = $1 AND temporary")
                .bind(channel_id)
                .execute(&state.db)
                .await?
                .rows_affected();
            if deleted > 0 {
                state.events.group(
                    EventKind::ChannelDeleted,
                    group_id,
                    json!({ "id": channel_id }),
                );
            }
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}

/// Maps a LiveKit identity (`name@domain`) to a local user id.
async fn local_user(
    state: &AppState,
    participant: Option<&Participant>,
) -> AppResult<Option<Uuid>> {
    let Some(id) = participant.and_then(|p| p.identity.parse::<UserId>().ok()) else {
        return Ok(None);
    };
    if !id.is_local_to(&state.config.domain) {
        return Ok(None);
    }
    Ok(sqlx::query_scalar("SELECT id FROM users WHERE name = $1")
        .bind(id.name())
        .fetch_optional(&state.db)
        .await?)
}
