//! Accounts, password hashing and bearer-token sessions.

use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tinlink_protocol::UserId;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

const SESSION_TTL: Duration = Duration::days(30);
const MIN_PASSWORD_LEN: usize = 8;

/// The authenticated user of a request, extracted from `Authorization: Bearer`.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub name: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(AppError::Unauthorized)?;
        authenticate(&state.db, token)
            .await?
            .ok_or(AppError::Unauthorized)
    }
}

/// Resolves a session token to its user, if the session is valid.
pub async fn authenticate(db: &PgPool, token: &str) -> AppResult<Option<AuthUser>> {
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT u.id, u.name FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
    )
    .bind(hash_token(token))
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(id, name)| AuthUser { id, name }))
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub display_name: Option<String>,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub name: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub token: String,
    pub user: UserInfo,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserInfo {
    pub id: Uuid,
    pub name: String,
    pub display_name: String,
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> AppResult<(StatusCode, Json<SessionResponse>)> {
    if !state.config.open_registration {
        let any_user: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users)")
            .fetch_one(&state.db)
            .await?;
        // The very first account can always be created, so a fresh server has an admin.
        if any_user {
            return Err(AppError::Forbidden);
        }
    }
    let id = UserId::new(&req.name, &state.config.domain)
        .map_err(|e| AppError::bad_request(e.to_string()))?;
    if req.password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::bad_request(format!(
            "password must have at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let display_name = req
        .display_name
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| req.name.clone());
    let password_hash = hash_password(req.password).await?;

    let user: UserInfo = sqlx::query_as(
        "INSERT INTO users (id, name, display_name, password_hash) VALUES ($1, $2, $3, $4)
         RETURNING id, name, display_name",
    )
    .bind(Uuid::now_v7())
    .bind(id.name())
    .bind(&display_name)
    .bind(&password_hash)
    .fetch_one(&state.db)
    .await
    .map_err(|e| match AppError::from(e) {
        AppError::Conflict(_) => AppError::Conflict("user name is taken".into()),
        other => other,
    })?;

    let token = create_session(&state, user.id).await?;
    Ok((StatusCode::CREATED, Json(SessionResponse { token, user })))
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> AppResult<Json<SessionResponse>> {
    let key = req.name.to_ascii_lowercase();
    if let Some(wait) = state.login_limiter.blocked_for(&key) {
        return Err(AppError::TooManyRequests(wait.as_secs()));
    }
    let row: Option<(Uuid, String, String, String)> =
        sqlx::query_as("SELECT id, name, display_name, password_hash FROM users WHERE name = $1")
            .bind(req.name.to_ascii_lowercase())
            .fetch_optional(&state.db)
            .await?;
    // Verify against a dummy hash for unknown users to keep timing similar.
    let (stored_hash, user) = match row {
        Some((id, name, display_name, hash)) => (
            hash,
            Some(UserInfo {
                id,
                name,
                display_name,
            }),
        ),
        None => (DUMMY_HASH.clone(), None),
    };
    let valid = verify_password(req.password, stored_hash).await?;
    let Some(user) = user.filter(|_| valid) else {
        state.login_limiter.record_failure(&key);
        return Err(AppError::Unauthorized);
    };
    state.login_limiter.reset(&key);
    let token = create_session(&state, user.id).await?;
    Ok(Json(SessionResponse { token, user }))
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> AppResult<StatusCode> {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(hash_token(token))
            .execute(&state.db)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<UserInfo>> {
    let info = sqlx::query_as("SELECT id, name, display_name FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(info))
}

#[derive(Debug, Deserialize)]
pub struct ChangePassword {
    pub current_password: String,
    pub new_password: String,
}

/// Changes the password and signs out all other sessions.
pub async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    Json(req): Json<ChangePassword>,
) -> AppResult<StatusCode> {
    let key = user.name.to_ascii_lowercase();
    if let Some(wait) = state.login_limiter.blocked_for(&key) {
        return Err(AppError::TooManyRequests(wait.as_secs()));
    }
    if req.new_password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::bad_request(format!(
            "password must have at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let stored: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if !verify_password(req.current_password, stored).await? {
        state.login_limiter.record_failure(&key);
        return Err(AppError::bad_request("current password is wrong"));
    }
    let hash = hash_password(req.new_password).await?;
    let current = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(hash_token)
        .unwrap_or_default();
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user.id)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
        .bind(user.id)
        .bind(current)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_session(state: &AppState, user_id: Uuid) -> AppResult<String> {
    let token = random_token()?;
    sqlx::query("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, $3)")
        .bind(hash_token(&token))
        .bind(user_id)
        .bind(OffsetDateTime::now_utc() + SESSION_TTL)
        .execute(&state.db)
        .await?;
    Ok(token)
}

/// 256 bit random token, base64url encoded.
pub fn random_token() -> AppResult<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("getrandom failed: {e}"))?;
    Ok(B64.encode(buf))
}

fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// A real Argon2 hash, verified for unknown users so that login takes about
/// the same time whether or not the account exists.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    Argon2::default()
        .hash_password(b"tinlink-dummy-password")
        .expect("hashing a constant password works")
        .to_string()
});

async fn hash_password(password: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("password hashing failed: {e}"))
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(AppError::from)
}

async fn verify_password(password: String, hash: String) -> AppResult<bool> {
    tokio::task::spawn_blocking(move || {
        let parsed =
            PasswordHash::new(&hash).map_err(|e| anyhow::anyhow!("invalid stored hash: {e}"))?;
        Ok::<_, anyhow::Error>(
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
        )
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(AppError::from)
}
