//! Public keys and wrapped channel keys for end-to-end encryption.
//!
//! The server is a mailbox here: it stores what clients upload and checks
//! who may fetch it, but cannot verify or read any of it. Clients check
//! every signature themselves (see docs/e2ee.md), so a wrong answer from
//! the server can hide keys but not forge them.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::access::{chat_access, chat_readers};
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::state::AppState;

const KEY_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;
/// An HPKE-wrapped 32 byte key is 32 + 32 + 16 bytes; leave room for
/// later formats.
const MAX_WRAPPED: usize = 256;
const MAX_IDENTITY_BACKUP: usize = 1024;
const MAX_DEVICE_NAME: usize = 64;
const MAX_KEYS_PER_REQUEST: usize = 500;

/// Binary data as standard base64 in JSON.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s)
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

fn check_len(name: &str, value: &Bytes, len: usize) -> AppResult<()> {
    if value.0.len() != len {
        return Err(AppError::bad_request(format!("{name} must be {len} bytes")));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SetIdentity {
    pub identity_key: Bytes,
    /// The identity private key encrypted with the recovery key.
    pub identity_backup: Bytes,
    /// Replace a different identity key. Revokes all devices: everyone
    /// has to trust the new key again.
    #[serde(default)]
    pub reset: bool,
}

/// Sets the caller's identity key once, or replaces it with `reset`.
pub async fn set_identity(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<SetIdentity>,
) -> AppResult<StatusCode> {
    check_len("identity_key", &req.identity_key, KEY_LEN)?;
    if req.identity_backup.0.is_empty() || req.identity_backup.0.len() > MAX_IDENTITY_BACKUP {
        return Err(AppError::bad_request("identity_backup has a wrong size"));
    }
    let mut tx = state.db.begin().await?;
    let current: Option<Bytes> =
        sqlx::query_scalar("SELECT identity_key FROM users WHERE id = $1 FOR UPDATE")
            .bind(user.id)
            .fetch_one(&mut *tx)
            .await?;
    if current.as_ref().is_some_and(|k| *k != req.identity_key) {
        if !req.reset {
            return Err(AppError::Conflict("identity key already set".into()));
        }
        sqlx::query(
            "UPDATE devices SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE users SET identity_key = $2, identity_backup = $3 WHERE id = $1")
        .bind(user.id)
        .bind(&req.identity_key)
        .bind(&req.identity_backup)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.events.keys_changed(user.id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct IdentityBackup {
    pub identity_key: Option<Bytes>,
    pub identity_backup: Option<Bytes>,
}

/// The caller's encrypted identity key, to restore it with the recovery key.
pub async fn identity_backup(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<IdentityBackup>> {
    let (identity_key, identity_backup) =
        sqlx::query_as("SELECT identity_key, identity_backup FROM users WHERE id = $1")
            .bind(user.id)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(IdentityBackup {
        identity_key,
        identity_backup,
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DeviceInfo {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub sign_key: Option<Bytes>,
    pub dh_key: Bytes,
    pub certificate: Option<Bytes>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct UserKeys {
    pub user_id: Uuid,
    pub identity_key: Option<Bytes>,
    /// Active devices. Others only see certified ones; the user also sees
    /// devices waiting for approval.
    pub devices: Vec<DeviceInfo>,
    /// Revoked devices with the identity key that had certified them, so
    /// what they signed stays checkable. Only in `GET /users/{id}/keys`.
    pub former_devices: Vec<FormerDevice>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FormerDevice {
    pub id: Uuid,
    pub sign_key: Bytes,
    pub dh_key: Bytes,
    pub certificate: Bytes,
    pub identity_key: Bytes,
}

/// A user's identity key and devices. Visible to the user and to everyone
/// who shares a group or a direct message channel with them.
pub async fn user_keys(
    State(state): State<AppState>,
    user: AuthUser,
    Path(user_id): Path<Uuid>,
) -> AppResult<Json<UserKeys>> {
    let is_self = user_id == user.id;
    if !is_self {
        let related: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM members x JOIN members y ON x.group_id = y.group_id
                            WHERE x.user_id = $1 AND y.user_id = $2)
                 OR EXISTS (SELECT 1 FROM direct_channels
                            WHERE (user_a = $1 AND user_b = $2) OR (user_a = $2 AND user_b = $1))",
        )
        .bind(user.id)
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
        if !related {
            return Err(AppError::NotFound);
        }
    }
    let mut keys = load_user_keys(&state, user_id, is_self).await?;
    keys.former_devices = sqlx::query_as(
        "SELECT id, sign_key, dh_key, certificate, identity_key FROM devices
         WHERE user_id = $1 AND revoked_at IS NOT NULL AND kind = 'device'
           AND sign_key IS NOT NULL AND certificate IS NOT NULL AND identity_key IS NOT NULL
         ORDER BY created_at",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(keys))
}

/// A user's identity key and active devices; pending ones only with
/// `with_pending`.
async fn load_user_keys(
    state: &AppState,
    user_id: Uuid,
    with_pending: bool,
) -> AppResult<UserKeys> {
    let identity_key: Option<Option<Bytes>> =
        sqlx::query_scalar("SELECT identity_key FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let identity_key = identity_key.ok_or(AppError::NotFound)?;
    let devices = sqlx::query_as(
        "SELECT id, kind, name, sign_key, dh_key, certificate, created_at FROM devices
         WHERE user_id = $1 AND revoked_at IS NULL AND ($2 OR certificate IS NOT NULL)
         ORDER BY created_at",
    )
    .bind(user_id)
    .bind(with_pending)
    .fetch_all(&state.db)
    .await?;
    Ok(UserKeys {
        user_id,
        identity_key,
        devices,
        former_devices: Vec::new(),
    })
}

/// Keys of everyone who can read the channel: who a new epoch goes to.
pub async fn channel_devices(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
) -> AppResult<Json<Vec<UserKeys>>> {
    chat_access(&state.db, channel_id, user.id).await?;
    let mut out = Vec::new();
    for reader in chat_readers(&state.db, channel_id).await? {
        out.push(load_user_keys(&state, reader, false).await?);
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct CreateDevice {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub sign_key: Option<Bytes>,
    pub dh_key: Bytes,
    pub certificate: Option<Bytes>,
}

/// Registers a device of the caller, or the recovery key's backup device.
pub async fn create_device(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<CreateDevice>,
) -> AppResult<StatusCode> {
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > MAX_DEVICE_NAME {
        return Err(AppError::bad_request("device name must be 1-64 characters"));
    }
    match (req.kind.as_str(), &req.sign_key) {
        ("device", Some(key)) => check_len("sign_key", key, KEY_LEN)?,
        // The backup only receives keys and is created by a device that
        // already holds the identity key.
        ("backup", None) if req.certificate.is_some() => {}
        _ => return Err(AppError::bad_request("invalid device kind or keys")),
    }
    check_len("dh_key", &req.dh_key, KEY_LEN)?;
    if let Some(cert) = &req.certificate {
        check_len("certificate", cert, SIGNATURE_LEN)?;
    }
    sqlx::query(
        "INSERT INTO devices (id, user_id, kind, name, sign_key, dh_key, certificate, identity_key)
         SELECT $1, $2, $3, $4, $5, $6, $7, CASE WHEN $7 IS NULL THEN NULL ELSE identity_key END
         FROM users WHERE id = $2",
    )
    .bind(req.id)
    .bind(user.id)
    .bind(&req.kind)
    .bind(name)
    .bind(&req.sign_key)
    .bind(&req.dh_key)
    .bind(&req.certificate)
    .execute(&state.db)
    .await?;
    state.events.keys_changed(user.id);
    Ok(StatusCode::CREATED)
}

#[derive(Debug, Deserialize)]
pub struct Certify {
    pub certificate: Bytes,
}

/// Approves one of the caller's devices with a signature of the identity key.
pub async fn certify_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path(device_id): Path<Uuid>,
    Json(req): Json<Certify>,
) -> AppResult<StatusCode> {
    check_len("certificate", &req.certificate, SIGNATURE_LEN)?;
    let updated = sqlx::query(
        "UPDATE devices SET certificate = $3,
                identity_key = (SELECT identity_key FROM users WHERE id = $2)
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(device_id)
    .bind(user.id)
    .bind(&req.certificate)
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    state.events.keys_changed(user.id);
    Ok(StatusCode::NO_CONTENT)
}

/// Revokes one of the caller's devices. Channels it could read get a new
/// key the next time someone writes there.
pub async fn revoke_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path(device_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let updated = sqlx::query(
        "UPDATE devices SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(device_id)
    .bind(user.id)
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    state.events.keys_changed(user.id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct DeviceQuery {
    pub device: Uuid,
}

/// Fails unless `device_id` is an active device of the caller.
async fn own_device(state: &AppState, user: &AuthUser, device_id: Uuid) -> AppResult<()> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM devices
                        WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL)",
    )
    .bind(device_id)
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;
    if ok { Ok(()) } else { Err(AppError::NotFound) }
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct WrappedKey {
    pub epoch: i32,
    pub sender_id: Uuid,
    /// Whose identity key certifies the sending device.
    pub sender_user_id: Uuid,
    pub wrapped: Bytes,
    pub signature: Bytes,
}

#[derive(Debug, Serialize)]
pub struct ChannelKeys {
    /// The newest epoch, which new messages use; None before the first key.
    pub epoch: Option<i32>,
    /// Someone who held the newest key lost access: the next sender has to
    /// create a new epoch first.
    pub rotate: bool,
    /// All keys wrapped to the requesting device, oldest first.
    pub keys: Vec<WrappedKey>,
}

async fn current_epoch(db: &sqlx::PgPool, channel_id: Uuid) -> AppResult<Option<i32>> {
    Ok(
        sqlx::query_scalar("SELECT max(epoch) FROM channel_key_epochs WHERE channel_id = $1")
            .bind(channel_id)
            .fetch_one(db)
            .await?,
    )
}

/// The channel keys wrapped to one of the caller's devices.
pub async fn channel_keys(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<DeviceQuery>,
) -> AppResult<Json<ChannelKeys>> {
    chat_access(&state.db, channel_id, user.id).await?;
    own_device(&state, &user, q.device).await?;
    let epoch = current_epoch(&state.db, channel_id).await?;
    let rotate = match epoch {
        Some(epoch) => {
            let readers = chat_readers(&state.db, channel_id).await?;
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM channel_keys k JOIN devices d ON d.id = k.device_id
                                WHERE k.channel_id = $1 AND k.epoch = $2
                                  AND (d.revoked_at IS NOT NULL OR NOT d.user_id = ANY($3)))",
            )
            .bind(channel_id)
            .bind(epoch)
            .bind(&readers)
            .fetch_one(&state.db)
            .await?
        }
        None => false,
    };
    let keys = sqlx::query_as(
        "SELECT k.epoch, k.sender_id, d.user_id AS sender_user_id, k.wrapped, k.signature
         FROM channel_keys k JOIN devices d ON d.id = k.sender_id
         WHERE k.channel_id = $1 AND k.device_id = $2 ORDER BY k.epoch",
    )
    .bind(channel_id)
    .bind(q.device)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(ChannelKeys {
        epoch,
        rotate,
        keys,
    }))
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MissingKeys {
    pub device_id: Uuid,
    pub user_id: Uuid,
    /// Epochs the requesting device holds and this one lacks.
    pub epochs: Vec<i32>,
}

/// Devices allowed to read the channel that lack keys the caller's device
/// could share with them: new members, new devices, the backup.
pub async fn missing_keys(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Query(q): Query<DeviceQuery>,
) -> AppResult<Json<Vec<MissingKeys>>> {
    chat_access(&state.db, channel_id, user.id).await?;
    own_device(&state, &user, q.device).await?;
    let readers = chat_readers(&state.db, channel_id).await?;
    let missing = sqlx::query_as(
        "SELECT d.id AS device_id, d.user_id, array_agg(mine.epoch ORDER BY mine.epoch) AS epochs
         FROM devices d
         CROSS JOIN (SELECT epoch FROM channel_keys WHERE channel_id = $1 AND device_id = $2) mine
         WHERE d.user_id = ANY($3) AND d.revoked_at IS NULL AND d.certificate IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM channel_keys k
                           WHERE k.channel_id = $1 AND k.epoch = mine.epoch AND k.device_id = d.id)
         GROUP BY d.id, d.user_id
         ORDER BY d.user_id, d.id",
    )
    .bind(channel_id)
    .bind(q.device)
    .bind(&readers)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(missing))
}

#[derive(Debug, Deserialize)]
pub struct NewKey {
    pub device_id: Uuid,
    pub wrapped: Bytes,
    pub signature: Bytes,
}

#[derive(Debug, Deserialize)]
pub struct AddKeys {
    /// The caller's sending device.
    pub device_id: Uuid,
    /// The newest epoch plus one starts a new epoch; an existing epoch
    /// shares that key with more devices.
    pub epoch: i32,
    pub keys: Vec<NewKey>,
}

#[derive(Debug, Serialize)]
pub struct Added {
    pub epoch: i32,
    pub added: u64,
}

/// Stores channel keys wrapped to devices of people who can read the channel.
pub async fn add_keys(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<AddKeys>,
) -> AppResult<(StatusCode, Json<Added>)> {
    let access = chat_access(&state.db, channel_id, user.id).await?;
    if req.keys.is_empty() || req.keys.len() > MAX_KEYS_PER_REQUEST {
        return Err(AppError::bad_request("1-500 keys per request"));
    }
    for key in &req.keys {
        check_len("signature", &key.signature, SIGNATURE_LEN)?;
        if key.wrapped.0.is_empty() || key.wrapped.0.len() > MAX_WRAPPED {
            return Err(AppError::bad_request("wrapped key has a wrong size"));
        }
    }
    let sender_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM devices
                        WHERE id = $1 AND user_id = $2 AND kind = 'device'
                          AND revoked_at IS NULL AND certificate IS NOT NULL)",
    )
    .bind(req.device_id)
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;
    if !sender_ok {
        return Err(AppError::Forbidden);
    }

    let readers = chat_readers(&state.db, channel_id).await?;
    let recipients: Vec<Uuid> = req.keys.iter().map(|k| k.device_id).collect();
    let allowed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM devices
         WHERE id = ANY($1) AND user_id = ANY($2) AND revoked_at IS NULL AND certificate IS NOT NULL",
    )
    .bind(&recipients)
    .bind(&readers)
    .fetch_one(&state.db)
    .await?;
    let mut unique = recipients.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != recipients.len() || allowed as usize != unique.len() {
        return Err(AppError::bad_request(
            "keys may only go to approved devices of people who can read the channel",
        ));
    }

    let mut tx = state.db.begin().await?;
    // Serializes epoch changes per channel.
    sqlx::query("SELECT id FROM channels WHERE id = $1 FOR UPDATE")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;
    let current: Option<i32> =
        sqlx::query_scalar("SELECT max(epoch) FROM channel_key_epochs WHERE channel_id = $1")
            .bind(channel_id)
            .fetch_one(&mut *tx)
            .await?;
    let next = current.unwrap_or(0) + 1;
    if req.epoch == next {
        if !recipients.contains(&req.device_id) {
            return Err(AppError::bad_request(
                "a new epoch must include the sending device",
            ));
        }
        sqlx::query(
            "INSERT INTO channel_key_epochs (channel_id, epoch, created_by) VALUES ($1, $2, $3)",
        )
        .bind(channel_id)
        .bind(req.epoch)
        .bind(req.device_id)
        .execute(&mut *tx)
        .await?;
    } else if req.epoch < 1 || req.epoch > next {
        return Err(AppError::Conflict(format!(
            "epoch {} does not follow the newest epoch",
            req.epoch
        )));
    } else {
        // Held by any of the caller's devices, including the backup a
        // device just opened with the recovery key.
        let holds: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM channel_keys k JOIN devices d ON d.id = k.device_id
                            WHERE k.channel_id = $1 AND k.epoch = $2 AND d.user_id = $3)",
        )
        .bind(channel_id)
        .bind(req.epoch)
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await?;
        if !holds {
            return Err(AppError::Forbidden);
        }
    }
    let mut added = 0;
    for key in &req.keys {
        added += sqlx::query(
            "INSERT INTO channel_keys (channel_id, epoch, device_id, sender_id, wrapped, signature)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
        )
        .bind(channel_id)
        .bind(req.epoch)
        .bind(key.device_id)
        .bind(req.device_id)
        .bind(&key.wrapped)
        .bind(&key.signature)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    let status = if req.epoch == next {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    if added > 0 {
        state.events.chat(
            EventKind::ChannelKeysAdded,
            access.group_id,
            channel_id,
            serde_json::json!({ "epoch": req.epoch }),
        );
    }
    Ok((
        status,
        Json(Added {
            epoch: req.epoch,
            added,
        }),
    ))
}
