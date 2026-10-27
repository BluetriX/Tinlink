//! Avatars and group/channel icons.
//!
//! Clients scale images down and send them as a data URL in JSON, so every
//! client can use its plain JSON API path. Images are served without
//! authentication under a random id; a changed icon gets a new id.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use tinlink_protocol::Permissions;
use uuid::Uuid;

use super::channels::{ChannelInfo, ChannelRow, channel_columns};
use super::groups::{GroupInfo, group_columns};
use crate::access::{Member, channel_access};
use crate::auth::{AuthUser, UserInfo, user_columns};
use crate::error::{AppError, AppResult};
use crate::events::EventKind;
use crate::state::AppState;

/// Plenty for a 256x256 WebP or PNG.
pub const MAX_IMAGE_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
pub struct SetImage {
    /// `data:image/...;base64,...`, or null to remove the image.
    pub image: Option<String>,
}

/// Decodes a data URL and returns the content type detected from the bytes;
/// the type claimed in the URL is ignored.
pub fn decode_image(data_url: &str) -> AppResult<(&'static str, Vec<u8>)> {
    let b64 = data_url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .map(|(_, b64)| b64)
        .ok_or_else(|| AppError::bad_request("image must be a base64 data URL"))?;
    if b64.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
        return Err(too_large());
    }
    let data = STANDARD
        .decode(b64)
        .map_err(|_| AppError::bad_request("image is not valid base64"))?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err(too_large());
    }
    let content_type = sniff(&data)
        .ok_or_else(|| AppError::bad_request("image must be PNG, JPEG, GIF or WebP"))?;
    Ok((content_type, data))
}

fn too_large() -> AppError {
    AppError::bad_request(format!(
        "image is larger than {} KB",
        MAX_IMAGE_BYTES / 1024
    ))
}

fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Stores the new image (if any) and returns its id.
async fn store(tx: &mut Transaction<'_, Postgres>, image: Option<&str>) -> AppResult<Option<Uuid>> {
    let Some(image) = image else {
        return Ok(None);
    };
    let (content_type, data) = decode_image(image)?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO images (id, content_type, data) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(content_type)
        .bind(data)
        .execute(&mut **tx)
        .await?;
    Ok(Some(id))
}

async fn remove(tx: &mut Transaction<'_, Postgres>, old: Option<Uuid>) -> AppResult<()> {
    if let Some(old) = old {
        sqlx::query("DELETE FROM images WHERE id = $1")
            .bind(old)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Sets or removes the caller's avatar.
pub async fn set_avatar(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<SetImage>,
) -> AppResult<Json<UserInfo>> {
    let mut tx = state.db.begin().await?;
    let old: Option<Uuid> =
        sqlx::query_scalar("SELECT avatar_id FROM users WHERE id = $1 FOR UPDATE")
            .bind(user.id)
            .fetch_one(&mut *tx)
            .await?;
    let new = store(&mut tx, req.image.as_deref()).await?;
    let info: UserInfo = sqlx::query_as(concat!(
        "UPDATE users SET avatar_id = $2 WHERE id = $1 RETURNING ",
        user_columns!()
    ))
    .bind(user.id)
    .bind(new)
    .fetch_one(&mut *tx)
    .await?;
    remove(&mut tx, old).await?;
    tx.commit().await?;
    state.events.user_updated(&info);
    Ok(Json(info))
}

/// Sets or removes a group's icon (MANAGE_GROUP).
pub async fn set_group_icon(
    State(state): State<AppState>,
    user: AuthUser,
    Path(group_id): Path<Uuid>,
    Json(req): Json<SetImage>,
) -> AppResult<Json<GroupInfo>> {
    Member::load(&state.db, group_id, user.id)
        .await?
        .require(Permissions::MANAGE_GROUP)?;
    let mut tx = state.db.begin().await?;
    let old: Option<Uuid> =
        sqlx::query_scalar("SELECT icon_id FROM groups WHERE id = $1 FOR UPDATE")
            .bind(group_id)
            .fetch_one(&mut *tx)
            .await?;
    let new = store(&mut tx, req.image.as_deref()).await?;
    let group: GroupInfo = sqlx::query_as(concat!(
        "UPDATE groups SET icon_id = $2 WHERE id = $1 RETURNING ",
        group_columns!()
    ))
    .bind(group_id)
    .bind(new)
    .fetch_one(&mut *tx)
    .await?;
    remove(&mut tx, old).await?;
    tx.commit().await?;
    state
        .events
        .group(EventKind::GroupUpdated, group_id, &group);
    Ok(Json(group))
}

/// Sets or removes a channel's icon (MANAGE_CHANNELS).
pub async fn set_channel_icon(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    Json(req): Json<SetImage>,
) -> AppResult<Json<ChannelInfo>> {
    let (member, perms) = channel_access(&state.db, channel_id, user.id).await?;
    if !perms.contains(Permissions::MANAGE_CHANNELS) {
        return Err(AppError::Forbidden);
    }
    let mut tx = state.db.begin().await?;
    let old: Option<Uuid> =
        sqlx::query_scalar("SELECT icon_id FROM channels WHERE id = $1 FOR UPDATE")
            .bind(channel_id)
            .fetch_one(&mut *tx)
            .await?;
    let new = store(&mut tx, req.image.as_deref()).await?;
    let channel: ChannelRow = sqlx::query_as(concat!(
        "UPDATE channels SET icon_id = $2 WHERE id = $1 RETURNING ",
        channel_columns!()
    ))
    .bind(channel_id)
    .bind(new)
    .fetch_one(&mut *tx)
    .await?;
    remove(&mut tx, old).await?;
    tx.commit().await?;
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

/// Serves an image. Ids are random and never reused, so it is cached forever.
pub async fn get(State(state): State<AppState>, Path(id): Path<Uuid>) -> AppResult<Response> {
    let (content_type, data): (String, Vec<u8>) =
        sqlx::query_as("SELECT content_type, data FROM images WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(data))
        .map_err(|e| AppError::Internal(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_types_from_bytes() {
        let png = format!(
            "data:image/gif;base64,{}",
            STANDARD.encode(b"\x89PNG\r\n\x1a\nrest")
        );
        assert_eq!(decode_image(&png).unwrap().0, "image/png");
        let webp = format!("data:x;base64,{}", STANDARD.encode(b"RIFF\0\0\0\0WEBPVP8 "));
        assert_eq!(decode_image(&webp).unwrap().0, "image/webp");
        let svg = format!("data:image/svg+xml;base64,{}", STANDARD.encode(b"<svg/>"));
        assert!(decode_image(&svg).is_err());
        assert!(decode_image("https://example.org/a.png").is_err());
    }

    #[test]
    fn rejects_large_images() {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(MAX_IMAGE_BYTES + 1, 0);
        let url = format!("data:image/png;base64,{}", STANDARD.encode(data));
        assert!(decode_image(&url).is_err());
    }
}
