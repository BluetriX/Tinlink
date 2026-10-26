//! Periodic maintenance tasks.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use crate::events::EventKind;
use crate::files;
use crate::state::AppState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Temporary channels nobody joined are removed after this age. Channels
/// that were used are removed by the `room_finished` webhook instead.
pub const UNUSED_TEMPORARY_CHANNEL_AGE: Duration = Duration::from_secs(10 * 60);
/// Uploads not linked to a message (or whose message was deleted) are
/// removed after this age.
pub const ORPHAN_ATTACHMENT_AGE: Duration = Duration::from_secs(60 * 60);

pub fn spawn(state: AppState) {
    spawn_voice_sync(state.clone());
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) =
                sweep(&state, UNUSED_TEMPORARY_CHANNEL_AGE, ORPHAN_ATTACHMENT_AGE).await
            {
                tracing::warn!(error = ?err, "maintenance sweep failed");
            }
        }
    });
}

/// Deletes expired sessions, empty temporary channels older than
/// `channel_age` and orphaned uploads older than `attachment_age`.
pub async fn sweep(
    state: &AppState,
    channel_age: Duration,
    attachment_age: Duration,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
        .execute(&state.db)
        .await?;
    let deleted: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "DELETE FROM channels c WHERE c.temporary
           AND c.created_at < now() - make_interval(secs => $1)
           AND NOT EXISTS (SELECT 1 FROM voice_participants v WHERE v.channel_id = c.id)
         RETURNING c.id, c.group_id",
    )
    .bind(channel_age.as_secs_f64())
    .fetch_all(&state.db)
    .await?;
    for (channel_id, group_id) in deleted {
        state.events.group(
            EventKind::ChannelDeleted,
            group_id,
            json!({ "id": channel_id }),
        );
    }

    let orphans: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM attachments WHERE message_id IS NULL
           AND created_at < now() - make_interval(secs => $1)
         RETURNING id",
    )
    .bind(attachment_age.as_secs_f64())
    .fetch_all(&state.db)
    .await?;
    for id in orphans {
        if let Err(err) = tokio::fs::remove_file(files::path(state, id)).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%id, error = %err, "cannot remove orphaned upload");
        }
    }
    files::remove_unreferenced(state, attachment_age).await?;
    Ok(())
}

/// Applies permission changes to connected voice participants.
pub fn spawn_voice_sync(state: AppState) {
    let mut events = state.events.subscribe();
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let Some(group_id) = event.group_id else {
                continue;
            };
            let only_user = match event.kind {
                EventKind::GroupUpdated => None,
                EventKind::MemberLeft => event.user_id,
                _ => continue,
            };
            if let Err(err) = crate::voice_sync::refresh_group(&state, group_id, only_user).await {
                tracing::warn!(error = ?err, "voice permission sync failed");
            }
        }
    });
}
