//! Per-channel text chat.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tinlink_protocol::Permissions;
use uuid::Uuid;

use super::attachments::{self, AttachmentInfo};
use crate::access::channel_access;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::state::AppState;

const MAX_MESSAGE_LEN: usize = 4000;
const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 100;
const MAX_ATTACHMENTS: usize = 10;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MessageInfo {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_id: Option<Uuid>,
    pub author_name: Option<String>,
    pub content: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub edited_at: Option<OffsetDateTime>,
    #[sqlx(skip)]
    pub attachments: Vec<AttachmentInfo>,
}

/// Fills in the attachments of the given messages.
async fn attach(state: &AppState, messages: &mut [MessageInfo]) -> AppResult<()> {
    let ids: Vec<Uuid> = messages.iter().map(|m| m.id).collect();
    for a in attachments::for_messages(state, &ids).await? {
        if let Some(m) = messages.iter_mut().find(|m| Some(m.id) == a.message_id) {
            m.attachments.push(a);
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Return messages older than this message id.
    pub before: Option<Uuid>,
    pub limit: Option<i64>,
}

/// Returns messages newest first.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<MessageInfo>>> {
    channel_access(&state.db, channel_id, user.id).await?;
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let mut messages: Vec<MessageInfo> = sqlx::query_as(
        "SELECT m.id, m.channel_id, m.author_id, u.display_name AS author_name, m.content, m.created_at, m.edited_at
         FROM messages m LEFT JOIN users u ON u.id = m.author_id
         WHERE m.channel_id = $1 AND ($2::uuid IS NULL OR m.id < $2)
         ORDER BY m.id DESC LIMIT $3",
    )
    .bind(channel_id)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    attach(&state, &mut messages).await?;
    Ok(Json(messages))
}

#[derive(Debug, Deserialize)]
pub struct PostMessage {
    #[serde(default)]
    pub content: String,
    /// Uploaded attachments to link to this message.
    #[serde(default)]
    pub attachment_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct EditMessage {
    pub content: String,
}

fn validate_content(content: &str, allow_empty: bool) -> AppResult<&str> {
    let content = content.trim();
    if (content.is_empty() && !allow_empty) || content.chars().count() > MAX_MESSAGE_LEN {
        return Err(AppError::bad_request(format!(
            "message must be 1-{MAX_MESSAGE_LEN} characters"
        )));
    }
    Ok(content)
}

pub async fn post(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<PostMessage>,
) -> AppResult<(StatusCode, Json<MessageInfo>)> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if !perms.contains(Permissions::SEND_MESSAGES) {
        return Err(AppError::Forbidden);
    }
    if req.attachment_ids.len() > MAX_ATTACHMENTS {
        return Err(AppError::bad_request(format!(
            "at most {MAX_ATTACHMENTS} attachments"
        )));
    }
    let content = validate_content(&req.content, !req.attachment_ids.is_empty())?;
    let mut tx = state.db.begin().await?;
    let mut message: MessageInfo = sqlx::query_as(
        "WITH m AS (
            INSERT INTO messages (id, channel_id, author_id, content) VALUES ($1, $2, $3, $4) RETURNING *
         )
         SELECT m.id, m.channel_id, m.author_id, u.display_name AS author_name, m.content, m.created_at, m.edited_at
         FROM m LEFT JOIN users u ON u.id = m.author_id",
    )
    .bind(Uuid::now_v7())
    .bind(channel_id)
    .bind(user.id)
    .bind(content)
    .fetch_one(&mut *tx)
    .await?;
    if !req.attachment_ids.is_empty() {
        // Only the uploader's own, still unlinked uploads in this channel.
        let linked = sqlx::query(
            "UPDATE attachments SET message_id = $1
             WHERE id = ANY($2) AND channel_id = $3 AND uploader_id = $4 AND message_id IS NULL",
        )
        .bind(message.id)
        .bind(&req.attachment_ids)
        .bind(channel_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if linked != req.attachment_ids.len() as u64 {
            return Err(AppError::bad_request("unknown or already used attachment"));
        }
    }
    tx.commit().await?;
    attach(&state, std::slice::from_mut(&mut message)).await?;
    state.events.channel(
        EventKind::MessageCreated,
        member.group_id,
        channel_id,
        &message,
    );
    Ok((StatusCode::CREATED, Json(message)))
}

pub async fn edit(
    State(state): State<AppState>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditMessage>,
) -> AppResult<Json<MessageInfo>> {
    let (member, _) = channel_access(&state.db, channel_id, user.id).await?;
    let content = validate_content(&req.content, false)?;
    // Only the author can edit; anything else looks like a missing message.
    let mut message: MessageInfo = sqlx::query_as(
        "WITH m AS (
            UPDATE messages SET content = $4, edited_at = now()
            WHERE id = $1 AND channel_id = $2 AND author_id = $3 RETURNING *
         )
         SELECT m.id, m.channel_id, m.author_id, u.display_name AS author_name, m.content, m.created_at, m.edited_at
         FROM m LEFT JOIN users u ON u.id = m.author_id",
    )
    .bind(message_id)
    .bind(channel_id)
    .bind(user.id)
    .bind(content)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    attach(&state, std::slice::from_mut(&mut message)).await?;
    state.events.channel(
        EventKind::MessageUpdated,
        member.group_id,
        channel_id,
        &message,
    );
    Ok(Json(message))
}

/// Authors can delete their own messages; MANAGE_MESSAGES deletes any.
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    let author: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT author_id FROM messages WHERE id = $1 AND channel_id = $2")
            .bind(message_id)
            .bind(channel_id)
            .fetch_optional(&state.db)
            .await?;
    let author = author.ok_or(AppError::NotFound)?;
    if author != Some(user.id) && !perms.contains(Permissions::MANAGE_MESSAGES) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(message_id)
        .execute(&state.db)
        .await?;
    state.events.channel(
        EventKind::MessageDeleted,
        member.group_id,
        channel_id,
        serde_json::json!({ "id": message_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}
