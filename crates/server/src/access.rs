//! Loads membership and roles from the database and resolves permissions.

use sqlx::PgPool;
use tinlink_protocol::permissions::OverwriteTarget;
use tinlink_protocol::{Overwrite, Permissions, compute_channel_permissions};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

pub fn to_db(p: Permissions) -> i64 {
    p.bits() as i64
}

pub fn from_db(bits: i64) -> Permissions {
    Permissions::from_bits_truncate(bits as u64)
}

/// A user's membership in a group with all role data needed for checks.
#[derive(Debug, Clone)]
pub struct Member {
    pub group_id: Uuid,
    pub user_id: Uuid,
    pub is_owner: bool,
    pub everyone_perms: Permissions,
    /// Roles of the member except @everyone: (role id, position, permissions).
    pub roles: Vec<(Uuid, i32, Permissions)>,
}

impl Member {
    /// Loads the member, or returns `NotFound` if the user is not in the group.
    /// Not-found instead of forbidden avoids leaking which groups exist.
    pub async fn load(db: &PgPool, group_id: Uuid, user_id: Uuid) -> AppResult<Self> {
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT g.owner_id FROM groups g JOIN members m ON m.group_id = g.id
             WHERE g.id = $1 AND m.user_id = $2",
        )
        .bind(group_id)
        .bind(user_id)
        .fetch_optional(db)
        .await?;
        let owner = owner.ok_or(AppError::NotFound)?;

        let everyone: i64 = sqlx::query_scalar("SELECT permissions FROM roles WHERE id = $1")
            .bind(group_id)
            .fetch_one(db)
            .await?;
        let roles: Vec<(Uuid, i32, i64)> = sqlx::query_as(
            "SELECT r.id, r.position, r.permissions FROM member_roles mr JOIN roles r ON r.id = mr.role_id
             WHERE mr.group_id = $1 AND mr.user_id = $2",
        )
        .bind(group_id)
        .bind(user_id)
        .fetch_all(db)
        .await?;

        Ok(Self {
            group_id,
            user_id,
            is_owner: owner == user_id,
            everyone_perms: from_db(everyone),
            roles: roles
                .into_iter()
                .map(|(id, pos, p)| (id, pos, from_db(p)))
                .collect(),
        })
    }

    /// Group-wide permissions (no channel overwrites). The owner has all.
    pub fn group_permissions(&self) -> Permissions {
        if self.is_owner {
            return Permissions::all();
        }
        let base = self
            .roles
            .iter()
            .fold(self.everyone_perms, |acc, r| acc | r.2);
        if base.contains(Permissions::ADMINISTRATOR) {
            Permissions::all()
        } else {
            base
        }
    }

    /// Position of the member's highest role; the owner ranks above everything.
    pub fn highest_position(&self) -> i32 {
        if self.is_owner {
            return i32::MAX;
        }
        self.roles.iter().map(|r| r.1).max().unwrap_or(0)
    }

    pub fn require(&self, perm: Permissions) -> AppResult<()> {
        if self.group_permissions().contains(perm) {
            Ok(())
        } else {
            Err(AppError::Forbidden)
        }
    }

    /// Effective permissions in a channel, including overwrites.
    pub async fn channel_permissions(
        &self,
        db: &PgPool,
        channel_id: Uuid,
    ) -> AppResult<Permissions> {
        if self.is_owner {
            return Ok(Permissions::all());
        }
        let overwrites = load_overwrites(db, channel_id).await?;
        let roles: Vec<(Uuid, Permissions)> = self.roles.iter().map(|r| (r.0, r.2)).collect();
        Ok(compute_channel_permissions(
            &self.group_id,
            self.everyone_perms,
            &self.user_id,
            &roles,
            &overwrites,
        ))
    }
}

pub async fn load_overwrites(
    db: &PgPool,
    channel_id: Uuid,
) -> AppResult<Vec<Overwrite<Uuid, Uuid>>> {
    let rows: Vec<(String, Uuid, i64, i64)> = sqlx::query_as(
        "SELECT target_kind, target_id, allow_bits, deny_bits FROM overwrites WHERE channel_id = $1",
    )
    .bind(channel_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, id, allow, deny)| Overwrite {
            target: if kind == "role" {
                OverwriteTarget::Role(id)
            } else {
                OverwriteTarget::Member(id)
            },
            allow: from_db(allow),
            deny: from_db(deny),
        })
        .collect())
}

/// Looks up the group a channel belongs to.
/// Direct message channels have no group and are reported as not found.
pub async fn channel_group(db: &PgPool, channel_id: Uuid) -> AppResult<Uuid> {
    let group: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT group_id FROM channels WHERE id = $1")
            .bind(channel_id)
            .fetch_optional(db)
            .await?;
    group.flatten().ok_or(AppError::NotFound)
}

/// Loads the member for the group of `channel_id` and its channel permissions.
/// Channels the member cannot see are reported as not found.
pub async fn channel_access(
    db: &PgPool,
    channel_id: Uuid,
    user_id: Uuid,
) -> AppResult<(Member, Permissions)> {
    let group_id = channel_group(db, channel_id).await?;
    let member = Member::load(db, group_id, user_id).await?;
    let perms = member.channel_permissions(db, channel_id).await?;
    if !perms.contains(Permissions::VIEW_CHANNEL) {
        return Err(AppError::NotFound);
    }
    Ok((member, perms))
}

/// What both participants may do in a direct message channel: chat, but
/// not moderate.
pub const DIRECT_PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::ATTACH_FILES);

/// Access to a text chat, which is either a group channel or a direct
/// message channel.
pub struct ChatAccess {
    /// None for direct message channels.
    pub group_id: Option<Uuid>,
    /// The caller's membership, for group channels.
    pub member: Option<Member>,
    pub perms: Permissions,
}

/// Like `channel_access`, but also admits the participants of a direct
/// message channel. Used by everything that only deals with messages.
pub async fn chat_access(db: &PgPool, channel_id: Uuid, user_id: Uuid) -> AppResult<ChatAccess> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT c.group_id, d.user_a, d.user_b FROM channels c
         LEFT JOIN direct_channels d ON d.channel_id = c.id WHERE c.id = $1",
    )
    .bind(channel_id)
    .fetch_optional(db)
    .await?;
    match row.ok_or(AppError::NotFound)? {
        (Some(_), _, _) => {
            let (member, perms) = channel_access(db, channel_id, user_id).await?;
            Ok(ChatAccess {
                group_id: Some(member.group_id),
                member: Some(member),
                perms,
            })
        }
        (None, a, b) if a == Some(user_id) || b == Some(user_id) => Ok(ChatAccess {
            group_id: None,
            member: None,
            perms: DIRECT_PERMISSIONS,
        }),
        _ => Err(AppError::NotFound),
    }
}

/// Whether the user is one of the two participants of a direct message channel.
pub async fn is_direct_participant(db: &PgPool, channel_id: Uuid, user_id: Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM direct_channels
                        WHERE channel_id = $1 AND (user_a = $2 OR user_b = $2))",
    )
    .bind(channel_id)
    .bind(user_id)
    .fetch_one(db)
    .await
    .unwrap_or(false)
}

/// Everyone who can read a chat: the members of the channel's group with
/// VIEW_CHANNEL, or the two participants of a direct message channel.
pub async fn chat_readers(db: &PgPool, channel_id: Uuid) -> AppResult<Vec<Uuid>> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT c.group_id, d.user_a, d.user_b FROM channels c
         LEFT JOIN direct_channels d ON d.channel_id = c.id WHERE c.id = $1",
    )
    .bind(channel_id)
    .fetch_optional(db)
    .await?;
    let group_id = match row.ok_or(AppError::NotFound)? {
        (Some(group_id), _, _) => group_id,
        (None, a, b) => return Ok(a.into_iter().chain(b).collect()),
    };
    let users: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM members WHERE group_id = $1")
        .bind(group_id)
        .fetch_all(db)
        .await?;
    let mut readers = Vec::with_capacity(users.len());
    for user_id in users {
        let member = Member::load(db, group_id, user_id).await?;
        if member
            .channel_permissions(db, channel_id)
            .await?
            .contains(Permissions::VIEW_CHANNEL)
        {
            readers.push(user_id);
        }
    }
    Ok(readers)
}
