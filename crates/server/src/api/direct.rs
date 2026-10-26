//! Direct messages between two users. A direct message channel is a channel
//! without a group; messages, attachments and read markers work as in group
//! channels (see `access::chat_access`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct DirectUser {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
    pub online: bool,
}

#[derive(Debug, Serialize)]
pub struct DirectChannel {
    pub id: Uuid,
    /// The other participant.
    pub user: DirectUser,
    pub last_message_id: Option<Uuid>,
    pub last_read_id: Option<Uuid>,
    /// Unread messages from the other participant.
    pub unread_count: i64,
}

type Row = (Uuid, Uuid, String, String, Option<Uuid>, Option<Uuid>, i64);

/// The caller's direct message channels; `$2` optionally limits it to one.
const SELECT: &str = "
    SELECT d.channel_id, u.id, u.name, u.display_name,
           (SELECT m.id FROM messages m WHERE m.channel_id = d.channel_id ORDER BY m.id DESC LIMIT 1)
               AS last_message_id,
           r.last_read_id,
           (SELECT count(*) FROM messages m
            WHERE m.channel_id = d.channel_id AND m.author_id IS DISTINCT FROM $1
              AND (r.last_read_id IS NULL OR m.id > r.last_read_id))
    FROM direct_channels d
    JOIN users u ON u.id = CASE WHEN d.user_a = $1 THEN d.user_b ELSE d.user_a END
    LEFT JOIN read_states r ON r.channel_id = d.channel_id AND r.user_id = $1
    WHERE (d.user_a = $1 OR d.user_b = $1) AND ($2::uuid IS NULL OR d.channel_id = $2)
    ORDER BY last_message_id DESC NULLS LAST";

fn to_channel(state: &AppState, row: Row) -> DirectChannel {
    let (id, user_id, name, display_name, last_message_id, last_read_id, unread_count) = row;
    DirectChannel {
        id,
        user: DirectUser {
            id: user_id,
            name,
            display_name,
            online: state.presence.is_online(user_id),
        },
        last_message_id,
        last_read_id,
        unread_count,
    }
}

/// Lists the caller's direct message channels, most recent first.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Vec<DirectChannel>>> {
    let rows: Vec<Row> = sqlx::query_as(SELECT)
        .bind(user.id)
        .bind(None::<Uuid>)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(
        rows.into_iter().map(|r| to_channel(&state, r)).collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct OpenRequest {
    pub user_id: Uuid,
}

/// Opens the direct message channel with another user, creating it on first
/// use. New conversations need a shared group, so strangers cannot write to
/// each other; existing ones stay open.
pub async fn open(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<OpenRequest>,
) -> AppResult<(StatusCode, Json<DirectChannel>)> {
    if req.user_id == user.id {
        return Err(AppError::bad_request("cannot message yourself"));
    }
    let (a, b) = if user.id < req.user_id {
        (user.id, req.user_id)
    } else {
        (req.user_id, user.id)
    };
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT channel_id FROM direct_channels WHERE user_a = $1 AND user_b = $2",
    )
    .bind(a)
    .bind(b)
    .fetch_optional(&state.db)
    .await?;
    let (status, channel_id) = match existing {
        Some(id) => (StatusCode::OK, id),
        None => {
            let shares_group: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM members x JOIN members y ON x.group_id = y.group_id
                                WHERE x.user_id = $1 AND y.user_id = $2)",
            )
            .bind(user.id)
            .bind(req.user_id)
            .fetch_one(&state.db)
            .await?;
            if !shares_group {
                return Err(AppError::NotFound);
            }
            (StatusCode::CREATED, create(&state, a, b).await?)
        }
    };
    let row: Row = sqlx::query_as(SELECT)
        .bind(user.id)
        .bind(channel_id)
        .fetch_one(&state.db)
        .await?;
    Ok((status, Json(to_channel(&state, row))))
}

/// Creates the channel for a pair, or returns the one a concurrent request
/// created first.
async fn create(state: &AppState, a: Uuid, b: Uuid) -> AppResult<Uuid> {
    let mut tx = state.db.begin().await?;
    let channel_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO channels (id, group_id, name, voice_enabled) VALUES ($1, NULL, 'direct', false)",
    )
    .bind(channel_id)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO direct_channels (channel_id, user_a, user_b) VALUES ($1, $2, $3)
         ON CONFLICT (user_a, user_b) DO NOTHING",
    )
    .bind(channel_id)
    .bind(a)
    .bind(b)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        tx.commit().await?;
        return Ok(channel_id);
    }
    tx.rollback().await?;
    Ok(sqlx::query_scalar(
        "SELECT channel_id FROM direct_channels WHERE user_a = $1 AND user_b = $2",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&state.db)
    .await?)
}
