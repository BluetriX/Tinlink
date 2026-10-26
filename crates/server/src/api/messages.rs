//! Per-channel text chat.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tinlink_protocol::Permissions;
use uuid::Uuid;

use super::attachments::{self, AttachmentInfo};
use crate::access::{Member, chat_access};
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
    /// Users mentioned with `@name`.
    #[sqlx(skip)]
    pub mentions: Vec<Uuid>,
}

/// Fills in the attachments and mentions of the given messages.
async fn attach(state: &AppState, messages: &mut [MessageInfo]) -> AppResult<()> {
    let ids: Vec<Uuid> = messages.iter().map(|m| m.id).collect();
    for a in attachments::for_messages(state, &ids).await? {
        if let Some(m) = messages.iter_mut().find(|m| Some(m.id) == a.message_id) {
            m.attachments.push(a);
        }
    }
    let mentions: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT message_id, user_id FROM message_mentions WHERE message_id = ANY($1) ORDER BY user_id",
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    for (message_id, user_id) in mentions {
        if let Some(m) = messages.iter_mut().find(|m| m.id == message_id) {
            m.mentions.push(user_id);
        }
    }
    Ok(())
}

/// Lowercased user names mentioned with `@name`, without duplicates. A
/// mention starts at the beginning or after a character that cannot be part
/// of a name, so e-mail addresses do not count. Trailing dots and dashes are
/// treated as punctuation.
pub fn mentioned_names(content: &str) -> Vec<String> {
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut names: Vec<String> = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in content.char_indices() {
        if c == '@' && !prev.is_some_and(is_name_char) {
            let rest = &content[i + 1..];
            let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
            let name = rest[..end]
                .trim_end_matches(['.', '-'])
                .to_ascii_lowercase();
            if (1..=32).contains(&name.len()) && !names.contains(&name) {
                names.push(name);
            }
        }
        prev = Some(c);
    }
    names
}

/// Stores the mentions of a message: group members other than the author
/// who can see the channel.
async fn store_mentions(
    state: &AppState,
    tx: &mut sqlx::PgConnection,
    member: &Member,
    channel_id: Uuid,
    message_id: Uuid,
    content: &str,
) -> AppResult<()> {
    sqlx::query("DELETE FROM message_mentions WHERE message_id = $1")
        .bind(message_id)
        .execute(&mut *tx)
        .await?;
    let names = mentioned_names(content);
    if names.is_empty() {
        return Ok(());
    }
    let candidates: Vec<Uuid> = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN members m ON m.user_id = u.id AND m.group_id = $1
         WHERE u.name = ANY($2) AND u.id <> $3",
    )
    .bind(member.group_id)
    .bind(&names)
    .bind(member.user_id)
    .fetch_all(&mut *tx)
    .await?;
    for user_id in candidates {
        let other = Member::load(&state.db, member.group_id, user_id).await?;
        let perms = other.channel_permissions(&state.db, channel_id).await?;
        if perms.contains(Permissions::VIEW_CHANNEL) {
            sqlx::query("INSERT INTO message_mentions (message_id, user_id) VALUES ($1, $2)")
                .bind(message_id)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    Ok(())
}

/// Moves a user's read marker forward (never back).
async fn mark_read(
    db: impl sqlx::PgExecutor<'_>,
    user_id: Uuid,
    channel_id: Uuid,
    message_id: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO read_states (user_id, channel_id, last_read_id) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, channel_id)
         DO UPDATE SET last_read_id = GREATEST(read_states.last_read_id, EXCLUDED.last_read_id)",
    )
    .bind(user_id)
    .bind(channel_id)
    .bind(message_id)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ReadRequest {
    pub message_id: Uuid,
}

/// Marks a channel as read up to a message.
pub async fn read(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<ReadRequest>,
) -> AppResult<StatusCode> {
    chat_access(&state.db, channel_id, user.id).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE id = $1 AND channel_id = $2)",
    )
    .bind(req.message_id)
    .bind(channel_id)
    .fetch_one(&state.db)
    .await?;
    if !exists {
        return Err(AppError::NotFound);
    }
    mark_read(&state.db, user.id, channel_id, req.message_id).await?;
    Ok(StatusCode::NO_CONTENT)
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
    chat_access(&state.db, channel_id, user.id).await?;
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
    let access = chat_access(&state.db, channel_id, user.id).await?;
    if !access.perms.contains(Permissions::SEND_MESSAGES) {
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
    if let Some(member) = &access.member {
        store_mentions(&state, &mut tx, member, channel_id, message.id, content).await?;
    }
    // Your own message is read by definition.
    mark_read(&mut *tx, user.id, channel_id, message.id).await?;
    tx.commit().await?;
    attach(&state, std::slice::from_mut(&mut message)).await?;
    state.events.chat(
        EventKind::MessageCreated,
        access.group_id,
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
    let access = chat_access(&state.db, channel_id, user.id).await?;
    let content = validate_content(&req.content, false)?;
    let mut tx = state.db.begin().await?;
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
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if let Some(member) = &access.member {
        store_mentions(&state, &mut tx, member, channel_id, message.id, content).await?;
    }
    tx.commit().await?;
    attach(&state, std::slice::from_mut(&mut message)).await?;
    state.events.chat(
        EventKind::MessageUpdated,
        access.group_id,
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
    let access = chat_access(&state.db, channel_id, user.id).await?;
    let author: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT author_id FROM messages WHERE id = $1 AND channel_id = $2")
            .bind(message_id)
            .bind(channel_id)
            .fetch_optional(&state.db)
            .await?;
    let author = author.ok_or(AppError::NotFound)?;
    if author != Some(user.id) && !access.perms.contains(Permissions::MANAGE_MESSAGES) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM messages WHERE id = $1")
        .bind(message_id)
        .execute(&state.db)
        .await?;
    state.events.chat(
        EventKind::MessageDeleted,
        access.group_id,
        channel_id,
        serde_json::json!({ "id": message_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::mentioned_names;

    #[test]
    fn finds_mentions() {
        assert_eq!(
            mentioned_names("@Alice hi @bob.smith, and @alice again."),
            ["alice", "bob.smith"]
        );
        assert_eq!(
            mentioned_names("(@carl) @dave- @eve."),
            ["carl", "dave", "eve"]
        );
    }

    #[test]
    fn ignores_non_mentions() {
        assert!(mentioned_names("mail me at erik@example.org").is_empty());
        assert!(mentioned_names("@ @. @-- @@").is_empty());
        assert!(mentioned_names(&format!("@{}", "a".repeat(33))).is_empty());
    }
}
