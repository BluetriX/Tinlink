//! Roles and role assignment.
//!
//! Hierarchy rule: a member can only create, edit, assign or remove roles
//! ranked below their own highest role, and can only grant permissions they
//! have themselves. The group owner is exempt.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tinlink_protocol::Permissions;
use uuid::Uuid;

use super::groups::validate_name;
use crate::access::{Member, from_db, to_db};
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct RoleInfo {
    pub id: Uuid,
    pub name: String,
    pub color: Option<i32>,
    pub position: i32,
    pub permissions: Permissions,
    /// True for the group's @everyone role (same id as the group).
    pub everyone: bool,
}

type RoleRow = (Uuid, Uuid, String, Option<i32>, i32, i64);

fn role_info((id, group_id, name, color, position, perms): RoleRow) -> RoleInfo {
    RoleInfo {
        id,
        name,
        color,
        position,
        permissions: from_db(perms),
        everyone: id == group_id,
    }
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
) -> AppResult<Json<Vec<RoleInfo>>> {
    Member::load(&state.db, group_id, user.id).await?;
    let rows: Vec<RoleRow> = sqlx::query_as(
        "SELECT id, group_id, name, color, position, permissions FROM roles WHERE group_id = $1
         ORDER BY position DESC",
    )
    .bind(group_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(role_info).collect()))
}

#[derive(Debug, Deserialize)]
pub struct RoleBody {
    pub name: Option<String>,
    pub color: Option<i32>,
    pub position: Option<i32>,
    pub permissions: Option<Permissions>,
}

fn check_grant(
    member: &Member,
    perms: Option<Permissions>,
    position: Option<i32>,
) -> AppResult<()> {
    if let Some(p) = perms
        && !member.group_permissions().contains(p)
    {
        return Err(AppError::Forbidden);
    }
    if let Some(pos) = position {
        if pos < 1 {
            return Err(AppError::bad_request("position must be at least 1"));
        }
        if pos >= member.highest_position() {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
    Json(req): Json<RoleBody>,
) -> AppResult<(StatusCode, Json<RoleInfo>)> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    member.require(Permissions::MANAGE_ROLES)?;
    let name = validate_name(req.name.as_deref().unwrap_or_default())?;
    let position = req.position.unwrap_or(1);
    let perms = req.permissions.unwrap_or_default();
    check_grant(&member, Some(perms), Some(position))?;

    let row: RoleRow = sqlx::query_as(
        "INSERT INTO roles (id, group_id, name, color, position, permissions) VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, group_id, name, color, position, permissions",
    )
    .bind(Uuid::now_v7())
    .bind(group_id)
    .bind(name)
    .bind(req.color)
    .bind(position)
    .bind(to_db(perms))
    .fetch_one(&state.db)
    .await?;
    state.events.group(EventKind::GroupUpdated, group_id, ());
    Ok((StatusCode::CREATED, Json(role_info(row))))
}

async fn load_role(state: &AppState, group_id: Uuid, role_id: Uuid) -> AppResult<i32> {
    sqlx::query_scalar("SELECT position FROM roles WHERE id = $1 AND group_id = $2")
        .bind(role_id)
        .bind(group_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, role_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<RoleBody>,
) -> AppResult<Json<RoleInfo>> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    member.require(Permissions::MANAGE_ROLES)?;
    let current_position = load_role(&state, group_id, role_id).await?;
    let is_everyone = role_id == group_id;
    if !is_everyone && current_position >= member.highest_position() {
        return Err(AppError::Forbidden);
    }
    if is_everyone && (req.position.is_some() || req.name.is_some()) {
        return Err(AppError::bad_request(
            "@everyone can only change permissions",
        ));
    }
    check_grant(&member, req.permissions, req.position)?;
    let name = req.name.as_deref().map(validate_name).transpose()?;

    let row: RoleRow = sqlx::query_as(
        "UPDATE roles SET name = COALESCE($2, name), color = COALESCE($3, color),
                position = COALESCE($4, position), permissions = COALESCE($5, permissions)
         WHERE id = $1 RETURNING id, group_id, name, color, position, permissions",
    )
    .bind(role_id)
    .bind(name)
    .bind(req.color)
    .bind(req.position)
    .bind(req.permissions.map(to_db))
    .fetch_one(&state.db)
    .await?;
    state.events.group(EventKind::GroupUpdated, group_id, ());
    Ok(Json(role_info(row)))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, role_id)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    member.require(Permissions::MANAGE_ROLES)?;
    if role_id == group_id {
        return Err(AppError::bad_request("@everyone cannot be deleted"));
    }
    if load_role(&state, group_id, role_id).await? >= member.highest_position() {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM roles WHERE id = $1")
        .bind(role_id)
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM overwrites WHERE target_kind = 'role' AND target_id = $1")
        .bind(role_id)
        .execute(&state.db)
        .await?;
    state.events.group(EventKind::GroupUpdated, group_id, ());
    Ok(StatusCode::NO_CONTENT)
}

async fn check_assign(
    state: &AppState,
    user: &AuthUser,
    group_id: Uuid,
    role_id: Uuid,
    target: Uuid,
) -> AppResult<()> {
    let member = Member::load(&state.db, group_id, user.id).await?;
    member.require(Permissions::MANAGE_ROLES)?;
    if role_id == group_id {
        return Err(AppError::bad_request("@everyone is implicit"));
    }
    if load_role(state, group_id, role_id).await? >= member.highest_position() {
        return Err(AppError::Forbidden);
    }
    // Target must be a member of the group.
    Member::load(&state.db, group_id, target).await?;
    Ok(())
}

pub async fn assign(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, target, role_id)): Path<(Uuid, Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    check_assign(&state, &user, group_id, role_id, target).await?;
    sqlx::query("INSERT INTO member_roles (group_id, user_id, role_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(group_id)
        .bind(target)
        .bind(role_id)
        .execute(&state.db)
        .await?;
    state.events.group(
        EventKind::GroupUpdated,
        group_id,
        json!({ "user_id": target }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unassign(
    State(state): State<AppState>,
    user: AuthUser,
    Path((group_id, target, role_id)): Path<(Uuid, Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    check_assign(&state, &user, group_id, role_id, target).await?;
    sqlx::query("DELETE FROM member_roles WHERE group_id = $1 AND user_id = $2 AND role_id = $3")
        .bind(group_id)
        .bind(target)
        .bind(role_id)
        .execute(&state.db)
        .await?;
    state.events.group(
        EventKind::GroupUpdated,
        group_id,
        json!({ "user_id": target }),
    );
    Ok(StatusCode::NO_CONTENT)
}
