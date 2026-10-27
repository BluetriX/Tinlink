//! Groups (comparable to Discord servers), membership and invites.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use tinlink_protocol::Permissions;
use uuid::Uuid;

use crate::access::{Member, to_db};
use crate::auth::{AuthUser, random_token};
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::state::AppState;

const MAX_NAME_LEN: usize = 100;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GroupInfo {
    pub id: Uuid,
    pub name: String,
    pub owner_id: Uuid,
    pub user_channel_limit: i32,
}

#[derive(Debug, Deserialize)]
pub struct CreateGroup {
    pub name: String,
}

pub fn validate_name(name: &str) -> AppResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(AppError::bad_request(format!(
            "name must be 1-{MAX_NAME_LEN} characters"
        )));
    }
    Ok(name.to_owned())
}

/// Creates a group with an @everyone role, a default category for
/// user-created channels and a first channel.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<CreateGroup>,
) -> AppResult<(StatusCode, Json<GroupInfo>)> {
    let name = validate_name(&req.name)?;
    let group_id = Uuid::now_v7();
    let everyone = Permissions::DEFAULT_EVERYONE | Permissions::CREATE_OWN_CHANNELS;

    let mut tx = state.db.begin().await?;
    let group: GroupInfo = sqlx::query_as(
        "INSERT INTO groups (id, name, owner_id) VALUES ($1, $2, $3)
         RETURNING id, name, owner_id, user_channel_limit",
    )
    .bind(group_id)
    .bind(&name)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO roles (id, group_id, name, position, permissions) VALUES ($1, $1, '@everyone', 0, $2)")
        .bind(group_id)
        .bind(to_db(everyone))
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO members (group_id, user_id) VALUES ($1, $2)")
        .bind(group_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO channels (id, group_id, name) VALUES ($1, $2, 'general')")
        .bind(Uuid::now_v7())
        .bind(group_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO categories (id, group_id, name, position, allow_user_channels) VALUES ($1, $2, 'Rooms', 1, true)",
    )
    .bind(Uuid::now_v7())
    .bind(group_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // Lets the owner's open gateway connections deliver this group's events.
    state
        .events
        .member(EventKind::MemberJoined, group_id, user.id);

    Ok((StatusCode::CREATED, Json(group)))
}

#[derive(Debug, Serialize)]
pub struct MyGroup {
    #[serde(flatten)]
    pub group: GroupInfo,
    /// Some visible channel has messages the caller has not read.
    pub unread: bool,
    /// Unread messages in visible channels that mention the caller.
    pub mention_count: i64,
}

pub async fn list_mine(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<Vec<MyGroup>>> {
    let groups: Vec<GroupInfo> = sqlx::query_as(
        "SELECT g.id, g.name, g.owner_id, g.user_channel_limit FROM groups g
         JOIN members m ON m.group_id = g.id WHERE m.user_id = $1 ORDER BY g.name",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    // Per channel: does it have unread messages, and how many unread mentions.
    let channels: Vec<(Uuid, Uuid, bool, i64)> = sqlx::query_as(
        "SELECT c.group_id, c.id,
                EXISTS (SELECT 1 FROM messages m WHERE m.channel_id = c.id
                        AND (r.last_read_id IS NULL OR m.id > r.last_read_id)),
                (SELECT count(*) FROM message_mentions mm JOIN messages m ON m.id = mm.message_id
                 WHERE mm.user_id = $1 AND m.channel_id = c.id
                   AND (r.last_read_id IS NULL OR m.id > r.last_read_id))
         FROM channels c JOIN members mb ON mb.group_id = c.group_id AND mb.user_id = $1
         LEFT JOIN read_states r ON r.channel_id = c.id AND r.user_id = $1",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let mut result = Vec::with_capacity(groups.len());
    for group in groups {
        let member = Member::load(&state.db, group.id, user.id).await?;
        let (mut unread, mut mention_count) = (false, 0);
        for (_, channel_id, has_unread, mentions) in channels
            .iter()
            .filter(|c| c.0 == group.id && (c.2 || c.3 > 0))
        {
            // Only channels the caller can still see count.
            if member
                .channel_permissions(&state.db, *channel_id)
                .await?
                .contains(Permissions::VIEW_CHANNEL)
            {
                unread |= *has_unread;
                mention_count += mentions;
            }
        }
        result.push(MyGroup {
            group,
            unread,
            mention_count,
        });
    }
    Ok(Json(result))
}

#[derive(Debug, Serialize)]
pub struct GroupDetail {
    #[serde(flatten)]
    pub group: GroupInfo,
    /// The caller's group-wide permissions, so clients can show or hide actions.
    pub permissions: Permissions,
    /// Position of the caller's highest role (the owner ranks above all);
    /// only roles and members below it can be managed.
    pub my_position: i32,
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
) -> AppResult<Json<GroupDetail>> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    let group =
        sqlx::query_as("SELECT id, name, owner_id, user_channel_limit FROM groups WHERE id = $1")
            .bind(group_id)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(GroupDetail {
        group,
        permissions: member.group_permissions(),
        my_position: member.highest_position(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateGroup {
    pub name: Option<String>,
    pub user_channel_limit: Option<i32>,
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
    Json(req): Json<UpdateGroup>,
) -> AppResult<Json<GroupInfo>> {
    Member::load(&state.db, group_id, user.id)
        .await?
        .require(Permissions::MANAGE_GROUP)?;
    let name = req.name.as_deref().map(validate_name).transpose()?;
    if req
        .user_channel_limit
        .is_some_and(|l| !(0..=50).contains(&l))
    {
        return Err(AppError::bad_request("user_channel_limit must be 0-50"));
    }
    let group = sqlx::query_as(
        "UPDATE groups SET name = COALESCE($2, name), user_channel_limit = COALESCE($3, user_channel_limit)
         WHERE id = $1 RETURNING id, name, owner_id, user_channel_limit",
    )
    .bind(group_id)
    .bind(name)
    .bind(req.user_channel_limit)
    .fetch_one(&state.db)
    .await?;
    state
        .events
        .group(EventKind::GroupUpdated, group_id, &group);
    Ok(Json(group))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    if !member.is_owner {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM groups WHERE id = $1")
        .bind(group_id)
        .execute(&state.db)
        .await?;
    state.events.group(EventKind::GroupDeleted, group_id, ());
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MemberInfo {
    pub user_id: Uuid,
    pub name: String,
    pub display_name: String,
    pub role_ids: Vec<Uuid>,
    #[sqlx(skip)]
    pub online: bool,
}

pub async fn list_members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
) -> AppResult<Json<Vec<MemberInfo>>> {
    Member::load(&state.db, group_id, user.id).await?;
    let mut members: Vec<MemberInfo> = sqlx::query_as(
        "SELECT u.id AS user_id, u.name, u.display_name,
                COALESCE(array_agg(mr.role_id) FILTER (WHERE mr.role_id IS NOT NULL), '{}') AS role_ids
         FROM members m JOIN users u ON u.id = m.user_id
         LEFT JOIN member_roles mr ON mr.group_id = m.group_id AND mr.user_id = m.user_id
         WHERE m.group_id = $1 GROUP BY u.id ORDER BY u.name",
    )
    .bind(group_id)
    .fetch_all(&state.db)
    .await?;
    for m in &mut members {
        m.online = state.presence.is_online(m.user_id);
    }
    Ok(Json(members))
}

/// Leaves a group, or removes another member with KICK_MEMBERS.
pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, target)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    if target == user.id {
        if member.is_owner {
            return Err(AppError::bad_request("the owner cannot leave the group"));
        }
    } else {
        member.require(Permissions::KICK_MEMBERS)?;
        let other = Member::load(&state.db, group_id, target).await?;
        if other.is_owner || other.highest_position() >= member.highest_position() {
            return Err(AppError::Forbidden);
        }
    }
    sqlx::query("DELETE FROM members WHERE group_id = $1 AND user_id = $2")
        .bind(group_id)
        .bind(target)
        .execute(&state.db)
        .await?;
    state.events.member(EventKind::MemberLeft, group_id, target);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct CreateInvite {
    pub max_uses: Option<i32>,
    pub expires_in_hours: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct InviteInfo {
    pub code: String,
    pub group_id: Uuid,
    pub uses: i32,
    pub max_uses: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

pub async fn create_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
    Json(req): Json<CreateInvite>,
) -> AppResult<(StatusCode, Json<InviteInfo>)> {
    Member::load(&state.db, group_id, user.id)
        .await?
        .require(Permissions::MANAGE_GROUP)?;
    let code: String = random_token()?.chars().take(12).collect();
    let expires_at = req
        .expires_in_hours
        .filter(|h| *h > 0)
        .map(|h| OffsetDateTime::now_utc() + Duration::hours(h));
    let invite = sqlx::query_as(
        "INSERT INTO invites (code, group_id, created_by, max_uses, expires_at) VALUES ($1, $2, $3, $4, $5)
         RETURNING code, group_id, uses, max_uses, expires_at",
    )
    .bind(code)
    .bind(group_id)
    .bind(user.id)
    .bind(req.max_uses.filter(|m| *m > 0))
    .bind(expires_at)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(invite)))
}

/// Lists the group's invites that can still be used.
pub async fn list_invites(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
) -> AppResult<Json<Vec<InviteInfo>>> {
    Member::load(&state.db, group_id, user.id)
        .await?
        .require(Permissions::MANAGE_GROUP)?;
    let invites = sqlx::query_as(
        "SELECT code, group_id, uses, max_uses, expires_at FROM invites
         WHERE group_id = $1 AND (expires_at IS NULL OR expires_at > now())
           AND (max_uses IS NULL OR uses < max_uses)
         ORDER BY created_at DESC",
    )
    .bind(group_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(invites))
}

pub async fn delete_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, code)): Path<(Uuid, String)>,
) -> AppResult<StatusCode> {
    Member::load(&state.db, group_id, user.id)
        .await?
        .require(Permissions::MANAGE_GROUP)?;
    let deleted = sqlx::query("DELETE FROM invites WHERE code = $1 AND group_id = $2")
        .bind(code)
        .bind(group_id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Joins the group of an invite. Joining twice is a no-op.
pub async fn join(
    State(state): State<AppState>,
    user: AuthUser,
    Path(code): Path<String>,
) -> AppResult<Json<GroupInfo>> {
    let mut tx = state.db.begin().await?;
    // Lock the invite row so concurrent joins cannot exceed max_uses.
    let invite: Option<(Uuid, i32, Option<i32>)> = sqlx::query_as(
        "SELECT group_id, uses, max_uses FROM invites
         WHERE code = $1 AND (expires_at IS NULL OR expires_at > now())
         FOR UPDATE",
    )
    .bind(&code)
    .fetch_optional(&mut *tx)
    .await?;
    let (group_id, uses, max_uses) = invite.ok_or(AppError::NotFound)?;

    let already_member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM members WHERE group_id = $1 AND user_id = $2)",
    )
    .bind(group_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    if !already_member {
        if max_uses.is_some_and(|max| uses >= max) {
            return Err(AppError::NotFound);
        }
        sqlx::query("INSERT INTO members (group_id, user_id) VALUES ($1, $2)")
            .bind(group_id)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE invites SET uses = uses + 1 WHERE code = $1")
            .bind(&code)
            .execute(&mut *tx)
            .await?;
    }
    let group =
        sqlx::query_as("SELECT id, name, owner_id, user_channel_limit FROM groups WHERE id = $1")
            .bind(group_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    if !already_member {
        state
            .events
            .member(EventKind::MemberJoined, group_id, user.id);
    }
    Ok(Json(group))
}
