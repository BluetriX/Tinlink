//! File uploads in channels.
//!
//! Files are stored on disk under their attachment id. Downloads use signed,
//! expiring URLs so that clients which cannot set headers (e.g. an `<img>`
//! in a web client) can load them; the URL is handed out only to users who
//! can see the message.

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use time::OffsetDateTime;
use tinlink_protocol::Permissions;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::access::chat_access;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use crate::files;
use crate::state::AppState;

/// Signed download URLs are valid for at least this long.
const URL_TTL_SECS: i64 = 24 * 60 * 60;
const MAX_FILENAME_LEN: usize = 200;
/// Types rendered inline by browsers without script execution.
const INLINE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "audio/ogg",
    "video/mp4",
    "video/webm",
];

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AttachmentInfo {
    pub id: Uuid,
    #[serde(skip)]
    pub message_id: Option<Uuid>,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    /// Signed download URL, relative to the server.
    #[sqlx(skip)]
    pub url: String,
}

impl AttachmentInfo {
    pub fn with_url(mut self, state: &AppState) -> Self {
        self.url = signed_url(state, self.id);
        self
    }
}

fn url_mac(state: &AppState) -> Hmac<Sha256> {
    // Derive a dedicated key so file URLs and LiveKit tokens never share one.
    let mut kdf = Hmac::<Sha256>::new_from_slice(state.config.livekit.api_secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    kdf.update(b"tinlink file urls v1");
    Hmac::<Sha256>::new_from_slice(&kdf.finalize().into_bytes())
        .expect("HMAC accepts keys of any length")
}

fn signature(state: &AppState, id: Uuid, exp: i64) -> Vec<u8> {
    let mut mac = url_mac(state);
    mac.update(format!("{id}:{exp}").as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn signed_url(state: &AppState, id: Uuid) -> String {
    // Round the expiry up to the hour so URLs stay stable and cacheable.
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let exp = (now + URL_TTL_SECS) / 3600 * 3600 + 3600;
    let sig = B64.encode(signature(state, id, exp));
    format!("/api/files/{id}?exp={exp}&sig={sig}")
}

/// Loads attachments of the given messages with fresh download URLs.
pub async fn for_messages(
    state: &AppState,
    message_ids: &[Uuid],
) -> AppResult<Vec<AttachmentInfo>> {
    let rows: Vec<AttachmentInfo> = sqlx::query_as(
        "SELECT id, message_id, filename, content_type, size_bytes FROM attachments
         WHERE message_id = ANY($1) ORDER BY created_at",
    )
    .bind(message_ids)
    .fetch_all(&state.db)
    .await?;
    Ok(rows.into_iter().map(|a| a.with_url(state)).collect())
}

fn sanitize_filename(raw: Option<&str>) -> String {
    let name = raw
        .unwrap_or("file")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("file");
    let name: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(MAX_FILENAME_LEN)
        .collect();
    let name = name.trim().to_owned();
    if name.is_empty() || name == "." || name == ".." {
        "file".into()
    } else {
        name
    }
}

fn sanitize_content_type(raw: Option<&str>) -> String {
    raw.and_then(|ct| ct.parse::<mime::Mime>().ok())
        .map(|m| m.essence_str().to_owned())
        .unwrap_or_else(|| "application/octet-stream".into())
}

/// Uploads one file (multipart field `file`). Link it to a message by
/// passing its id in `attachment_ids` when posting.
pub async fn upload(
    State(state): State<AppState>,
    user: AuthUser,
    Path(channel_id): Path<Uuid>,
    mut multipart: Multipart,
) -> AppResult<(StatusCode, Json<AttachmentInfo>)> {
    let access = chat_access(&state.db, channel_id, user.id).await?;
    if !access.perms.contains(Permissions::ATTACH_FILES) {
        return Err(AppError::Forbidden);
    }
    let mut field = loop {
        match multipart
            .next_field()
            .await
            .map_err(|e| AppError::bad_request(e.to_string()))?
        {
            Some(f) if f.name() == Some("file") => break f,
            Some(_) => continue,
            None => return Err(AppError::bad_request("multipart field 'file' is missing")),
        }
    };
    let filename = sanitize_filename(field.file_name());
    let content_type = sanitize_content_type(field.content_type());

    let id = Uuid::now_v7();
    let path = files::path(&state, id);
    tokio::fs::create_dir_all(&state.config.files_dir)
        .await
        .map_err(anyhow::Error::from)?;
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(anyhow::Error::from)?;
    let mut size = 0usize;
    let result: AppResult<()> = async {
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::bad_request(e.to_string()))?
        {
            size += chunk.len();
            if size > state.config.max_upload_bytes {
                return Err(AppError::bad_request(format!(
                    "file is larger than {} MB",
                    state.config.max_upload_bytes / 1024 / 1024
                )));
            }
            file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
        }
        file.flush().await.map_err(anyhow::Error::from)?;
        Ok(())
    }
    .await;
    if let Err(err) = result {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(err);
    }

    let info: AttachmentInfo = sqlx::query_as(
        "INSERT INTO attachments (id, channel_id, uploader_id, filename, content_type, size_bytes)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, message_id, filename, content_type, size_bytes",
    )
    .bind(id)
    .bind(channel_id)
    .bind(user.id)
    .bind(&filename)
    .bind(&content_type)
    .bind(size as i64)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(info.with_url(&state))))
}

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    exp: i64,
    sig: String,
}

/// Serves a file for a valid signed URL.
pub async fn download(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<DownloadQuery>,
) -> AppResult<Response> {
    let sig = B64.decode(&q.sig).map_err(|_| AppError::NotFound)?;
    let mut mac = url_mac(&state);
    mac.update(format!("{id}:{}", q.exp).as_bytes());
    if mac.verify_slice(&sig).is_err() || q.exp < OffsetDateTime::now_utc().unix_timestamp() {
        return Err(AppError::NotFound);
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT filename, content_type FROM attachments WHERE id = $1 AND message_id IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?;
    let (filename, content_type) = row.ok_or(AppError::NotFound)?;
    let bytes = tokio::fs::read(files::path(&state, id))
        .await
        .map_err(|_| AppError::NotFound)?;

    let inline = INLINE_TYPES.contains(&content_type.as_str());
    let served_type = if inline {
        content_type.as_str()
    } else {
        "application/octet-stream"
    };
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        percent_encode(&filename)
    );
    Response::builder()
        .header(header::CONTENT_TYPE, served_type)
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(e.into()))
}

/// RFC 5987 encoding for the filename in Content-Disposition.
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_sanitized() {
        assert_eq!(sanitize_filename(Some("../../etc/passwd")), "passwd");
        assert_eq!(
            sanitize_filename(Some("C:\\Users\\x\\photo.png")),
            "photo.png"
        );
        assert_eq!(sanitize_filename(Some("..")), "file");
        assert_eq!(sanitize_filename(Some("a\"b\nc.txt")), "abc.txt");
        assert_eq!(sanitize_filename(None), "file");
    }

    #[test]
    fn content_types_are_normalized() {
        assert_eq!(
            sanitize_content_type(Some("image/PNG; charset=x")),
            "image/png"
        );
        assert_eq!(
            sanitize_content_type(Some("not a type")),
            "application/octet-stream"
        );
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("Grüße 1.txt"), "Gr%C3%BC%C3%9Fe%201.txt");
    }
}
