//! Periodic maintenance tasks.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use crate::events::EventKind;
use crate::state::AppState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Temporary channels nobody joined are removed after this age. Channels
/// that were used are removed by the `room_finished` webhook instead.
pub const UNUSED_TEMPORARY_CHANNEL_AGE: Duration = Duration::from_secs(10 * 60);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = sweep(&state, UNUSED_TEMPORARY_CHANNEL_AGE).await {
                tracing::warn!(error = ?err, "maintenance sweep failed");
            }
        }
    });
}

/// Deletes expired sessions and empty temporary channels older than `min_age`.
pub async fn sweep(state: &AppState, min_age: Duration) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
        .execute(&state.db)
        .await?;
    let deleted: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "DELETE FROM channels c WHERE c.temporary
           AND c.created_at < now() - make_interval(secs => $1)
           AND NOT EXISTS (SELECT 1 FROM voice_participants v WHERE v.channel_id = c.id)
         RETURNING c.id, c.group_id",
    )
    .bind(min_age.as_secs_f64())
    .fetch_all(&state.db)
    .await?;
    for (channel_id, group_id) in deleted {
        state.events.group(
            EventKind::ChannelDeleted,
            group_id,
            json!({ "id": channel_id }),
        );
    }
    Ok(())
}
