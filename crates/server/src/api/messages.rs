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

/// Words per search query; more are ignored.
const MAX_SEARCH_WORDS: usize = 8;

/// Turns user input into a tsquery where every word must match as a prefix,
/// e.g. "Pizza fr" → "pizza:* & fr:*". Only letters and digits survive, so
/// the result cannot contain tsquery operators.
pub fn search_query(input: &str) -> Option<String> {
    let words: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(MAX_SEARCH_WORDS)
        .map(|w| format!("{}:*", w.to_lowercase()))
        .collect();
    (!words.is_empty()).then(|| words.join(" & "))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// Only this group's channels.
    pub group_id: Option<Uuid>,
    /// Only this channel (a group channel or a direct message channel).
    pub channel_id: Option<Uuid>,
    /// Hits older than this message id, for paging.
    pub before: Option<Uuid>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub message: MessageInfo,
    /// None for direct messages.
    pub group_id: Option<Uuid>,
    /// The channel's name, or the other participant's name for direct messages.
    pub channel_name: String,
}

/// Channels the user may search: visible channels of their groups (or of one
/// group) plus their direct message channels, or a single channel.
async fn searchable_channels(
    state: &AppState,
    user_id: Uuid,
    q: &SearchQuery,
) -> AppResult<Vec<Uuid>> {
    if let Some(channel_id) = q.channel_id {
        chat_access(&state.db, channel_id, user_id).await?;
        return Ok(vec![channel_id]);
    }
    let groups: Vec<Uuid> = match q.group_id {
        Some(id) => vec![id],
        None => {
            sqlx::query_scalar("SELECT group_id FROM members WHERE user_id = $1")
                .bind(user_id)
                .fetch_all(&state.db)
                .await?
        }
    };
    let mut channels = Vec::new();
    for group_id in groups {
        let member = Member::load(&state.db, group_id, user_id).await?;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM channels WHERE group_id = $1")
            .bind(group_id)
            .fetch_all(&state.db)
            .await?;
        for id in ids {
            if member
                .channel_permissions(&state.db, id)
                .await?
                .contains(Permissions::VIEW_CHANNEL)
            {
                channels.push(id);
            }
        }
    }
    if q.group_id.is_none() {
        let direct: Vec<Uuid> = sqlx::query_scalar(
            "SELECT channel_id FROM direct_channels WHERE user_a = $1 OR user_b = $1",
        )
        .bind(user_id)
        .fetch_all(&state.db)
        .await?;
        channels.extend(direct);
    }
    Ok(channels)
}

/// Full-text search over the messages the caller can see, newest first.
pub async fn search(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<Vec<SearchHit>>> {
    let Some(tsquery) = search_query(&q.q) else {
        return Err(AppError::bad_request("enter a search term"));
    };
    let channels = searchable_channels(&state, user.id, &q).await?;
    let limit = q.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let mut messages: Vec<MessageInfo> = sqlx::query_as(
        "SELECT m.id, m.channel_id, m.author_id, u.display_name AS author_name, m.content, m.created_at, m.edited_at
         FROM messages m LEFT JOIN users u ON u.id = m.author_id
         WHERE m.channel_id = ANY($1)
           AND to_tsvector('simple', m.content) @@ to_tsquery('simple', $2)
           AND ($3::uuid IS NULL OR m.id < $3)
         ORDER BY m.id DESC LIMIT $4",
    )
    .bind(&channels)
    .bind(&tsquery)
    .bind(q.before)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    attach(&state, &mut messages).await?;

    let ids: Vec<Uuid> = messages.iter().map(|m| m.channel_id).collect();
    let names: Vec<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT c.id, c.group_id, COALESCE(other.display_name, c.name)
         FROM channels c
         LEFT JOIN direct_channels d ON d.channel_id = c.id
         LEFT JOIN users other
                ON other.id = CASE WHEN d.user_a = $2 THEN d.user_b ELSE d.user_a END
         WHERE c.id = ANY($1)",
    )
    .bind(&ids)
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        messages
            .into_iter()
            .map(|message| {
                let (group_id, channel_name) = names
                    .iter()
                    .find(|n| n.0 == message.channel_id)
                    .map(|n| (n.1, n.2.clone()))
                    .unwrap_or_default();
                SearchHit {
                    message,
                    group_id,
                    channel_name,
                }
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{mentioned_names, search_query};

    #[test]
    fn builds_prefix_queries() {
        assert_eq!(search_query("Pizza fr").as_deref(), Some("pizza:* & fr:*"));
        assert_eq!(
            search_query("it's | !bad & (stuff):*").as_deref(),
            Some("it:* & s:* & bad:* & stuff:*")
        );
        assert_eq!(
            search_query("Grüße 2026").as_deref(),
            Some("grüße:* & 2026:*")
        );
        assert_eq!(search_query("  ?! "), None);
    }

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
