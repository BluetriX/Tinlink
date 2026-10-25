//! Keeps connected voice participants in line with their current
//! permissions: after role, overwrite or mute changes their LiveKit grants
//! are updated, and members who lost access are disconnected.

use tinlink_protocol::{Permissions, UserId};
use uuid::Uuid;

use crate::access::Member;
use crate::error::AppResult;
use crate::livekit::RoomServiceError;
use crate::state::AppState;

/// Effective voice permissions of a member in a channel, applying the
/// server-side mute.
pub async fn voice_permissions(
    state: &AppState,
    member: &Member,
    channel_id: Uuid,
) -> AppResult<Permissions> {
    let mut perms = member.channel_permissions(&state.db, channel_id).await?;
    let muted: bool =
        sqlx::query_scalar("SELECT voice_muted FROM members WHERE group_id = $1 AND user_id = $2")
            .bind(member.group_id)
            .bind(member.user_id)
            .fetch_one(&state.db)
            .await?;
    if muted {
        perms.remove(Permissions::SPEAK);
    }
    Ok(perms)
}

pub fn identity(state: &AppState, name: &str) -> String {
    UserId::new(name, &state.config.domain)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| format!("{name}@{}", state.config.domain))
}

/// Re-applies permissions to every participant connected in the group.
/// Optionally limited to one user.
pub async fn refresh_group(
    state: &AppState,
    group_id: Uuid,
    only_user: Option<Uuid>,
) -> AppResult<()> {
    let participants: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT v.channel_id, v.user_id, u.name FROM voice_participants v
         JOIN channels c ON c.id = v.channel_id JOIN users u ON u.id = v.user_id
         WHERE c.group_id = $1 AND ($2::uuid IS NULL OR v.user_id = $2)",
    )
    .bind(group_id)
    .bind(only_user)
    .fetch_all(&state.db)
    .await?;

    for (channel_id, user_id, name) in participants {
        let room = channel_id.to_string();
        let identity = identity(state, &name);
        let perms = match Member::load(&state.db, group_id, user_id).await {
            Ok(member) => voice_permissions(state, &member, channel_id).await?,
            // No longer a member of the group.
            Err(_) => Permissions::empty(),
        };
        let result = if perms.contains(Permissions::VIEW_CHANNEL | Permissions::CONNECT) {
            state
                .rooms
                .update_permissions(&room, &identity, perms)
                .await
        } else {
            state.rooms.remove(&room, &identity).await
        };
        match result {
            Ok(()) | Err(RoomServiceError::NotFound) | Err(RoomServiceError::NotConfigured) => {}
            Err(err) => {
                tracing::warn!(%channel_id, %user_id, error = %err, "cannot update voice participant")
            }
        }
    }
    Ok(())
}
