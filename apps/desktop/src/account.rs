//! Commands for the logged-in server connection: login, REST calls, files
//! and joining a channel's voice room.

use std::path::PathBuf;

use base64::Engine;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager, State};

use crate::backend::{Account, Backend, SavedSession, unique_path};
use crate::session::{MediaKey, TokenResponse};
use crate::{AppState, CmdResult, JoinInfo, StartupOptions, connect_voice, err};

/// Largest image shown inline in the chat.
const MAX_INLINE_IMAGE: usize = 8 * 1024 * 1024;

fn session_file(app: &AppHandle) -> CmdResult<PathBuf> {
    let name = match &app.state::<StartupOptions>().profile {
        Some(profile) => format!("session-{profile}.json"),
        None => "session.json".into(),
    };
    Ok(app.path().app_data_dir().map_err(err)?.join(name))
}

fn chain(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[derive(Serialize)]
pub struct BuildInfo {
    debug: bool,
    version: &'static str,
}

#[tauri::command]
pub fn build_info() -> BuildInfo {
    BuildInfo {
        debug: cfg!(debug_assertions),
        version: env!("CARGO_PKG_VERSION"),
    }
}

async fn activate(app: AppHandle, state: &AppState, mut backend: Backend) -> Account {
    backend.start_gateway(app);
    let account = backend.account();
    *state.backend.lock().await = Some(backend);
    account
}

/// Resumes the saved session, if any. An expired session is forgotten.
#[tauri::command]
pub async fn restore(app: AppHandle, state: State<'_, AppState>) -> CmdResult<Option<Account>> {
    let path = session_file(&app)?;
    let Some(saved) = SavedSession::load(&path) else {
        return Ok(None);
    };
    match Backend::resume(&saved).await {
        Ok(backend) => Ok(Some(activate(app, &state, backend).await)),
        Err(e) => {
            let msg = chain(e);
            if msg.contains("authentication required") {
                let _ = std::fs::remove_file(&path);
                return Ok(None);
            }
            Err(format!("cannot resume session on {}: {msg}", saved.server))
        }
    }
}

async fn authenticate(
    app: AppHandle,
    state: &AppState,
    server: &str,
    register: bool,
    body: Value,
) -> CmdResult<Account> {
    let (backend, saved) = Backend::authenticate(server, register, body)
        .await
        .map_err(chain)?;
    if let Err(e) = saved.save(&session_file(&app)?) {
        tracing::warn!("cannot store session: {e:#}");
    }
    Ok(activate(app, state, backend).await)
}

#[tauri::command]
pub async fn login(
    app: AppHandle,
    state: State<'_, AppState>,
    server: String,
    name: String,
    password: String,
) -> CmdResult<Account> {
    let body = json!({ "name": name.trim(), "password": password });
    authenticate(app, &state, &server, false, body).await
}

#[tauri::command]
pub async fn register(
    app: AppHandle,
    state: State<'_, AppState>,
    server: String,
    name: String,
    display_name: Option<String>,
    password: String,
) -> CmdResult<Account> {
    let display_name = display_name.filter(|n| !n.trim().is_empty());
    let body = json!({ "name": name.trim(), "display_name": display_name, "password": password });
    authenticate(app, &state, &server, true, body).await
}

#[tauri::command]
pub async fn logout(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(session) = state.session.lock().await.take() {
        session.leave(&state.voice).await;
    }
    if let Some(backend) = state.backend.lock().await.take() {
        backend.logout().await;
    }
    let _ = std::fs::remove_file(session_file(&app)?);
    Ok(())
}

/// Authenticated call to `/api{path}`, e.g. `api("GET", "/groups")`.
#[tauri::command]
pub async fn api(
    state: State<'_, AppState>,
    method: String,
    path: String,
    body: Option<Value>,
) -> CmdResult<Value> {
    let method: Method = method.to_uppercase().parse().map_err(err)?;
    let guard = state.backend.lock().await;
    let backend = guard.as_ref().ok_or("not logged in")?;
    backend.request(method, &path, body).await.map_err(chain)
}

/// Uploads a file sent as raw bytes; channel and file name come in the
/// `x-channel-id`, `x-filename` (URI-encoded) and `x-content-type` headers.
#[tauri::command]
pub async fn upload(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> CmdResult<Value> {
    let tauri::ipc::InvokeBody::Raw(data) = request.body() else {
        return Err("expected raw file data".into());
    };
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let channel_id = header("x-channel-id").ok_or("missing channel")?;
    let filename = header("x-filename")
        .map(|f| {
            percent_encoding::percent_decode_str(&f)
                .decode_utf8_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "file".into());
    let content_type =
        header("x-content-type").unwrap_or_else(|| "application/octet-stream".into());
    let guard = state.backend.lock().await;
    let backend = guard.as_ref().ok_or("not logged in")?;
    backend
        .upload(&channel_id, &filename, &content_type, data.clone())
        .await
        .map_err(chain)
}

/// Saves an attachment into the Downloads folder and returns the path.
#[tauri::command]
pub async fn download(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    filename: String,
) -> CmdResult<String> {
    let (_, data) = {
        let guard = state.backend.lock().await;
        let backend = guard.as_ref().ok_or("not logged in")?;
        backend.fetch(&url).await.map_err(chain)?
    };
    let dir = app.path().download_dir().map_err(err)?;
    let path = unique_path(&dir, &filename);
    std::fs::write(&path, data).map_err(err)?;
    Ok(path.display().to_string())
}

/// Returns an image attachment as a data URL for inline display.
#[tauri::command]
pub async fn image_data(state: State<'_, AppState>, url: String) -> CmdResult<String> {
    let (content_type, data) = {
        let guard = state.backend.lock().await;
        let backend = guard.as_ref().ok_or("not logged in")?;
        backend.fetch(&url).await.map_err(chain)?
    };
    if !content_type.starts_with("image/") || data.len() > MAX_INLINE_IMAGE {
        return Err("not an inline image".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    Ok(format!("data:{content_type};base64,{b64}"))
}

/// Tells others in the channel that this user is typing (the UI throttles).
#[tauri::command]
pub async fn send_typing(state: State<'_, AppState>, channel_id: String) -> CmdResult<()> {
    let guard = state.backend.lock().await;
    let backend = guard.as_ref().ok_or("not logged in")?;
    backend.gateway_send(json!({ "type": "typing", "channel_id": channel_id }));
    Ok(())
}

/// Joins the voice room of a channel.
#[tauri::command]
pub async fn join_voice(
    app: AppHandle,
    state: State<'_, AppState>,
    channel_id: String,
    media_keys: Option<Vec<MediaKey>>,
) -> CmdResult<JoinInfo> {
    let token = {
        let guard = state.backend.lock().await;
        let backend = guard.as_ref().ok_or("not logged in")?;
        backend.voice_token(&channel_id).await.map_err(chain)?
    };
    let token: TokenResponse = serde_json::from_value(token).map_err(err)?;
    connect_voice(app, &state, token, &media_keys.unwrap_or_default()).await
}

/// Hands newer media keys to the running call.
#[tauri::command]
pub async fn set_media_keys(state: State<'_, AppState>, keys: Vec<MediaKey>) -> CmdResult<()> {
    let guard = state.session.lock().await;
    let session = guard.as_ref().ok_or("not in voice")?;
    session.set_media_keys(&keys).map_err(chain)
}
