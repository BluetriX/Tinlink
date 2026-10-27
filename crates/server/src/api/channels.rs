//! Categories, channels and per-channel permission overwrites.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tinlink_protocol::Permissions;
use uuid::Uuid;

use super::groups::validate_name;
use crate::access::{Member, channel_access, load_overwrites, to_db};
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::state::AppState;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CategoryInfo {
    pub id: Uuid,
    pub name: String,
    pub position: i32,
    pub allow_user_channels: bool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ChannelRow {
    pub id: Uuid,
    pub group_id: Uuid,
    pub category_id: Option<Uuid>,
    pub name: String,
    pub position: i32,
    pub voice_enabled: bool,
    pub created_by: Option<Uuid>,
    pub temporary: bool,
}

#[derive(Debug, Serialize)]
pub struct ChannelInfo {
    #[serde(flatten)]
    pub channel: ChannelRow,
    /// The caller's effective permissions in this channel.
    pub permissions: Permissions,
    /// Only in the channel list; channel events go to many users.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub read_state: Option<ReadState>,
}

/// The caller's read position in a channel. The channel has unread messages
/// when `last_message_id` is greater than `last_read_id` (or nothing was read).
#[derive(Debug, Serialize)]
pub struct ReadState {
    pub last_message_id: Option<Uuid>,
    pub last_read_id: Option<Uuid>,
    /// Unread messages that mention the caller.
    pub mention_count: i64,
}

#[derive(Debug, Serialize)]
pub struct ChannelList {
    pub categories: Vec<CategoryInfo>,
    pub channels: Vec<ChannelInfo>,
    /// Who is connected to which voice channel (visible channels only).
    pub voice: Vec<VoiceParticipant>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct VoiceParticipant {
    pub channel_id: Uuid,
    pub user_id: Uuid,
}

macro_rules! channel_columns {
    () => {
        "id, group_id, category_id, name, position, voice_enabled, created_by, temporary"
    };
}

/// Lists categories and the channels the caller can see.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
) -> AppResult<Json<ChannelList>> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    let categories = sqlx::query_as(
        "SELECT id, name, position, allow_user_channels FROM categories WHERE group_id = $1 ORDER BY position, name",
    )
    .bind(group_id)
    .fetch_all(&state.db)
    .await?;
    let rows: Vec<ChannelRow> = sqlx::query_as(concat!(
        "SELECT ",
        channel_columns!(),
        " FROM channels WHERE group_id = $1 ORDER BY position, created_at"
    ))
    .bind(group_id)
    .fetch_all(&state.db)
    .await?;

    let mut channels = Vec::with_capacity(rows.len());
    for channel in rows {
        // Fine for small groups; batch overwrite loading once groups grow.
        let permissions = member.channel_permissions(&state.db, channel.id).await?;
        if permissions.contains(Permissions::VIEW_CHANNEL) {
            channels.push(ChannelInfo {
                channel,
                permissions,
                read_state: None,
            });
        }
    }
    let visible: Vec<Uuid> = channels.iter().map(|c| c.channel.id).collect();
    let read_states: Vec<(Uuid, Option<Uuid>, Option<Uuid>, i64)> = sqlx::query_as(
        "SELECT c.id,
                (SELECT m.id FROM messages m WHERE m.channel_id = c.id ORDER BY m.id DESC LIMIT 1),
                r.last_read_id,
                (SELECT count(*) FROM message_mentions mm JOIN messages m ON m.id = mm.message_id
                 WHERE mm.user_id = $2 AND m.channel_id = c.id
                   AND (r.last_read_id IS NULL OR m.id > r.last_read_id))
         FROM unnest($1::uuid[]) AS c(id)
         LEFT JOIN read_states r ON r.channel_id = c.id AND r.user_id = $2",
    )
    .bind(&visible)
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    for (id, last_message_id, last_read_id, mention_count) in read_states {
        if let Some(c) = channels.iter_mut().find(|c| c.channel.id == id) {
            c.read_state = Some(ReadState {
                last_message_id,
                last_read_id,
                mention_count,
            });
        }
    }
    let voice = sqlx::query_as(
        "SELECT channel_id, user_id FROM voice_participants WHERE channel_id = ANY($1) ORDER BY joined_at",
    )
    .bind(&visible)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(ChannelList {
        categories,
        channels,
        voice,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateCategory {
    pub name: String,
    #[serde(default)]
    pub allow_user_channels: bool,
    #[serde(default)]
    pub position: i32,
}

pub async fn create_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
    Json(req): Json<CreateCategory>,
) -> AppResult<(StatusCode, Json<CategoryInfo>)> {
    Member::load(&state.db, group_id, user.id)
        .await?
        .require(Permissions::MANAGE_CHANNELS)?;
    let category = sqlx::query_as(
        "INSERT INTO categories (id, group_id, name, position, allow_user_channels) VALUES ($1, $2, $3, $4, $5)
         RETURNING id, name, position, allow_user_channels",
    )
    .bind(Uuid::now_v7())
    .bind(group_id)
    .bind(validate_name(&req.name)?)
    .bind(req.position)
    .bind(req.allow_user_channels)
    .fetch_one(&state.db)
    .await?;
    state
        .events
        .group(EventKind::GroupUpdated, group_id, &category);
    Ok((StatusCode::CREATED, Json(category)))
}

/// Checks that a category belongs to the group.
async fn category_group(state: &AppState, category_id: Uuid, group_id: Uuid) -> AppResult<()> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM categories WHERE id = $1 AND group_id = $2)",
    )
    .bind(category_id)
    .bind(group_id)
    .fetch_one(&state.db)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(AppError::bad_request("unknown category"))
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateCategory {
    pub name: Option<String>,
    pub position: Option<i32>,
    pub allow_user_channels: Option<bool>,
}

/// Loads the member for the group of a category and requires MANAGE_CHANNELS.
async fn category_manager(state: &AppState, category_id: Uuid, user_id: Uuid) -> AppResult<Member> {
    let group_id: Uuid = sqlx::query_scalar("SELECT group_id FROM categories WHERE id = $1")
        .bind(category_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let member = Member::load(&state.db, group_id, user_id).await?;
    member.require(Permissions::MANAGE_CHANNELS)?;
    Ok(member)
}

pub async fn update_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(category_id): Path<Uuid>,
    Json(req): Json<UpdateCategory>,
) -> AppResult<Json<CategoryInfo>> {
    let member = category_manager(&state, category_id, user.id).await?;
    let name = req.name.as_deref().map(validate_name).transpose()?;
    let category = sqlx::query_as(
        "UPDATE categories SET name = COALESCE($2, name), position = COALESCE($3, position),
                allow_user_channels = COALESCE($4, allow_user_channels)
         WHERE id = $1 RETURNING id, name, position, allow_user_channels",
    )
    .bind(category_id)
    .bind(name)
    .bind(req.position)
    .bind(req.allow_user_channels)
    .fetch_one(&state.db)
    .await?;
    state
        .events
        .group(EventKind::GroupUpdated, member.group_id, &category);
    Ok(Json(category))
}

/// Deletes a category; its channels stay, without a category.
pub async fn delete_category(
    State(state): State<AppState>,
    user: AuthUser,
    Path(category_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let member = category_manager(&state, category_id, user.id).await?;
    sqlx::query("DELETE FROM categories WHERE id = $1")
        .bind(category_id)
        .execute(&state.db)
        .await?;
    state.events.group(
        EventKind::GroupUpdated,
        member.group_id,
        json!({ "category_id": category_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct CreateChannel {
    pub name: String,
    pub category_id: Option<Uuid>,
    #[serde(default = "default_true")]
    pub voice_enabled: bool,
    #[serde(default)]
    pub temporary: bool,
}

fn default_true() -> bool {
    true
}

/// Creates a channel. Members with MANAGE_CHANNELS create regular channels;
/// members with only CREATE_OWN_CHANNELS create "own" channels inside
/// categories that allow it, up to the group's per-member limit, and get
/// moderation rights in that channel.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
    Json(req): Json<CreateChannel>,
) -> AppResult<(StatusCode, Json<ChannelInfo>)> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    let name = validate_name(&req.name)?;
    let perms = member.group_permissions();

    let category: Option<(bool,)> = match req.category_id {
        Some(id) => Some(
            sqlx::query_as(
                "SELECT allow_user_channels FROM categories WHERE id = $1 AND group_id = $2",
            )
            .bind(id)
            .bind(group_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::bad_request("unknown category"))?,
        ),
        None => None,
    };

    let own_channel = if perms.contains(Permissions::MANAGE_CHANNELS) {
        false
    } else if perms.contains(Permissions::CREATE_OWN_CHANNELS) {
        if !matches!(category, Some((true,))) {
            return Err(AppError::bad_request(
                "own channels can only be created in categories that allow it",
            ));
        }
        true
    } else {
        return Err(AppError::Forbidden);
    };

    let mut tx = state.db.begin().await?;
    if own_channel {
        // Serialize concurrent creations by the same member to enforce the limit.
        sqlx::query("SELECT 1 FROM members WHERE group_id = $1 AND user_id = $2 FOR UPDATE")
            .bind(group_id)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
        let (count, limit): (i64, i32) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM channels WHERE group_id = $1 AND created_by = $2),
                    (SELECT user_channel_limit FROM groups WHERE id = $1)",
        )
        .bind(group_id)
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await?;
        if count >= i64::from(limit) {
            return Err(AppError::Conflict(format!(
                "limit of {limit} own channels reached"
            )));
        }
    }

    let channel: ChannelRow = sqlx::query_as(concat!(
        "INSERT INTO channels (id, group_id, category_id, name, voice_enabled, created_by, temporary)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        channel_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(group_id)
    .bind(req.category_id)
    .bind(&name)
    .bind(req.voice_enabled)
    .bind(own_channel.then_some(user.id))
    .bind(req.temporary && own_channel)
    .fetch_one(&mut *tx)
    .await?;

    if own_channel {
        sqlx::query(
            "INSERT INTO overwrites (channel_id, target_kind, target_id, allow_bits) VALUES ($1, 'member', $2, $3)",
        )
        .bind(channel.id)
        .bind(user.id)
        .bind(to_db(Permissions::CHANNEL_OWNER))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    state
        .events
        .channel(EventKind::ChannelCreated, group_id, channel.id, &channel);
    let permissions = member.channel_permissions(&state.db, channel.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(ChannelInfo {
            channel,
            permissions,
            read_state: None,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct UpdateChannel {
    pub name: Option<String>,
    pub position: Option<i32>,
    pub voice_enabled: Option<bool>,
    /// Missing: unchanged; null: no category; an id: move into that category.
    #[serde(default, deserialize_with = "some")]
    pub category_id: Option<Option<Uuid>>,
}

/// Lets a present `null` deserialize as `Some(None)`, unlike a missing field.
fn some<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<UpdateChannel>,
) -> AppResult<Json<ChannelInfo>> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if !perms.contains(Permissions::MANAGE_CHANNELS) {
        return Err(AppError::Forbidden);
    }
    let name = req.name.as_deref().map(validate_name).transpose()?;
    if let Some(Some(category_id)) = req.category_id {
        category_group(&state, category_id, member.group_id).await?;
    }
    let channel = sqlx::query_as(concat!(
        "UPDATE channels SET name = COALESCE($2, name), position = COALESCE($3, position),
                voice_enabled = COALESCE($4, voice_enabled),
                category_id = CASE WHEN $5 THEN $6 ELSE category_id END
         WHERE id = $1 RETURNING ",
        channel_columns!()
    ))
    .bind(channel_id)
    .bind(name)
    .bind(req.position)
    .bind(req.voice_enabled)
    .bind(req.category_id.is_some())
    .bind(req.category_id.flatten())
    .fetch_one(&state.db)
    .await?;
    state.events.channel(
        EventKind::ChannelUpdated,
        member.group_id,
        channel_id,
        &channel,
    );
    Ok(Json(ChannelInfo {
        channel,
        permissions: perms,
        read_state: None,
    }))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if !perms.contains(Permissions::MANAGE_CHANNELS) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM channels WHERE id = $1")
        .bind(channel_id)
        .execute(&state.db)
        .await?;
    state.events.group(
        EventKind::ChannelDeleted,
        member.group_id,
        json!({ "id": channel_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OverwriteBody {
    #[serde(default)]
    pub allow: Permissions,
    #[serde(default)]
    pub deny: Permissions,
}

pub async fn list_overwrites(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
) -> AppResult<Json<Vec<tinlink_protocol::Overwrite<Uuid, Uuid>>>> {
    channel_access(&state.db, channel_id, user.id).await?;
    Ok(Json(load_overwrites(&state.db, channel_id).await?))
}

/// Sets an overwrite for a role or member. Callers can only allow or deny
/// permissions they hold themselves in that channel.
pub async fn put_overwrite(
    State(state): State<AppState>,
    user: AuthUser,
    Path((channel_id, kind, target)): Path<(Uuid, String, Uuid)>,
    Json(body): Json<OverwriteBody>,
) -> AppResult<StatusCode> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if !perms.contains(Permissions::MANAGE_CHANNELS) {
        return Err(AppError::Forbidden);
    }
    if !perms.contains(body.allow | body.deny) {
        return Err(AppError::Forbidden);
    }
    validate_target(&state, member.group_id, &kind, target).await?;
    sqlx::query(
        "INSERT INTO overwrites (channel_id, target_kind, target_id, allow_bits, deny_bits) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (channel_id, target_kind, target_id)
         DO UPDATE SET allow_bits = EXCLUDED.allow_bits, deny_bits = EXCLUDED.deny_bits",
    )
    .bind(channel_id)
    .bind(&kind)
    .bind(target)
    .bind(to_db(body.allow))
    .bind(to_db(body.deny))
    .execute(&state.db)
    .await?;
    state.events.group(
        EventKind::GroupUpdated,
        member.group_id,
        json!({ "channel_id": channel_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_overwrite(
    State(state): State<AppState>,
    user: AuthUser,
    Path((channel_id, kind, target)): Path<(Uuid, String, Uuid)>,
) -> AppResult<StatusCode> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if !perms.contains(Permissions::MANAGE_CHANNELS) {
        return Err(AppError::Forbidden);
    }
    sqlx::query(
        "DELETE FROM overwrites WHERE channel_id = $1 AND target_kind = $2 AND target_id = $3",
    )
    .bind(channel_id)
    .bind(&kind)
    .bind(target)
    .execute(&state.db)
    .await?;
    state.events.group(
        EventKind::GroupUpdated,
        member.group_id,
        json!({ "channel_id": channel_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn validate_target(
    state: &AppState,
    group_id: Uuid,
    kind: &str,
    target: Uuid,
) -> AppResult<()> {
    let exists: bool = match kind {
        "role" => {
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM roles WHERE id = $1 AND group_id = $2)",
            )
            .bind(target)
            .bind(group_id)
            .fetch_one(&state.db)
            .await?
        }
        "member" => {
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM members WHERE user_id = $1 AND group_id = $2)",
            )
            .bind(target)
            .bind(group_id)
            .fetch_one(&state.db)
            .await?
        }
        _ => return Err(AppError::bad_request("kind must be 'role' or 'member'")),
    };
    if exists {
        Ok(())
    } else {
        Err(AppError::bad_request("unknown overwrite target"))
    }
}
