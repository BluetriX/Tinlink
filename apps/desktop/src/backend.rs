//! Connection to a Tinlink server: login, REST calls, file transfer and the
//! live event gateway.
//!
//! The session token stays in the Rust core and is stored in the app data
//! directory; the WebView never sees it. All HTTP goes through here, which
//! also lets debug builds accept the local stack's self-signed certificate.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// Accept the self-signed certificate of the local development stack.
const ACCEPT_INVALID_CERTS: bool = cfg!(debug_assertions);

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(ACCEPT_INVALID_CERTS)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("HTTP client configuration is valid")
}

/// Normalises user input such as "localhost" or "https://chat.example.org/".
pub fn normalize_server(server: &str) -> Result<String> {
    let server = server.trim().trim_end_matches('/');
    if server.is_empty() {
        bail!("server address is empty");
    }
    let url = if server.contains("://") {
        server.to_owned()
    } else {
        format!("https://{server}")
    };
    let parsed =
        reqwest::Url::parse(&url).with_context(|| format!("invalid server address {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("server address must start with http:// or https://");
    }
    Ok(url)
}

/// The gateway URL for a server URL (`https` → `wss`).
pub fn gateway_url(server: &str) -> String {
    let ws = if let Some(rest) = server.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        server.to_owned()
    };
    format!("{ws}/api/gateway")
}

/// Turns an error response (`{"error": "..."}`) into a readable message.
fn error_message(status: StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned());
    if detail.is_empty() {
        status.to_string()
    } else {
        detail
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("{}", error_message(status, &body)))
}

/// What is persisted between runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedSession {
    pub server: String,
    pub token: String,
}

impl SavedSession {
    pub fn load(path: &Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        serde_json::from_slice(&data).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }
}

/// Returned to the UI after login.
#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub server: String,
    pub user: Value,
}

pub struct Backend {
    client: reqwest::Client,
    server: String,
    token: String,
    user: Value,
    gateway: Option<JoinHandle<()>>,
    /// Messages for the gateway (e.g. typing notices); dropped while offline.
    gateway_tx: Option<mpsc::UnboundedSender<Value>>,
}

impl Backend {
    /// Logs in (or registers) and returns the session.
    pub async fn authenticate(
        server: &str,
        register: bool,
        body: Value,
    ) -> Result<(Self, SavedSession)> {
        let server = normalize_server(server)?;
        let client = http_client();
        let path = if register { "register" } else { "login" };
        let resp = client
            .post(format!("{server}/api/auth/{path}"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("cannot reach {server}"))?;
        let session: Value = check(resp).await?.json().await?;
        let token = session["token"]
            .as_str()
            .ok_or_else(|| anyhow!("server sent no token"))?
            .to_owned();
        let saved = SavedSession {
            server: server.clone(),
            token: token.clone(),
        };
        let backend = Self {
            client,
            server,
            token,
            user: session["user"].clone(),
            gateway: None,
            gateway_tx: None,
        };
        Ok((backend, saved))
    }

    /// Resumes a saved session if the token is still valid.
    pub async fn resume(saved: &SavedSession) -> Result<Self> {
        let mut backend = Self {
            client: http_client(),
            server: saved.server.clone(),
            token: saved.token.clone(),
            user: Value::Null,
            gateway: None,
            gateway_tx: None,
        };
        backend.user = backend.request(Method::GET, "/users/me", None).await?;
        Ok(backend)
    }

    pub fn account(&self) -> Account {
        Account {
            server: self.server.clone(),
            user: self.user.clone(),
        }
    }

    /// Calls `/api{path}` with the session token and returns the JSON body
    /// (`null` for empty responses).
    pub async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        if !path.starts_with('/') {
            bail!("API path must start with /");
        }
        let mut req = self
            .client
            .request(method, format!("{}/api{path}", self.server))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.context("server not reachable")?;
        let bytes = check(resp).await?.bytes().await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn upload(
        &self,
        channel_id: &str,
        filename: &str,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<Value> {
        let content_type =
            if content_type.contains('/') && !content_type.contains(char::is_whitespace) {
                content_type
            } else {
                "application/octet-stream"
            };
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.to_owned())
            .mime_str(content_type)?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let resp = self
            .client
            .post(format!(
                "{}/api/channels/{channel_id}/attachments",
                self.server
            ))
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .context("server not reachable")?;
        Ok(check(resp).await?.json().await?)
    }

    /// Fetches a signed attachment URL (relative to the server).
    pub async fn fetch(&self, url: &str) -> Result<(String, Vec<u8>)> {
        if !url.starts_with("/api/files/") {
            bail!("not an attachment URL");
        }
        let resp = self
            .client
            .get(format!("{}{url}", self.server))
            .send()
            .await
            .context("server not reachable")?;
        let resp = check(resp).await?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        Ok((content_type, resp.bytes().await?.to_vec()))
    }

    pub async fn voice_token(&self, channel_id: &str) -> Result<Value> {
        self.request(
            Method::POST,
            &format!("/channels/{channel_id}/voice-token"),
            None,
        )
        .await
    }

    pub async fn logout(&self) {
        let _ = self.request(Method::POST, "/auth/logout", None).await;
    }

    /// Starts forwarding gateway events to the UI as `gateway` events.
    pub fn start_gateway(&mut self, app: AppHandle) {
        let url = gateway_url(&self.server);
        let token = self.token.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        self.gateway_tx = Some(tx);
        self.gateway = Some(tokio::spawn(gateway_loop(app, url, token, rx)));
    }

    /// Sends a message over the gateway, e.g. `{"type":"typing",...}`.
    pub fn gateway_send(&self, message: Value) {
        if let Some(tx) = &self.gateway_tx {
            let _ = tx.send(message);
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(gateway) = self.gateway.take() {
            gateway.abort();
        }
    }
}

/// Keeps a gateway connection open, reconnecting with backoff. After a
/// reconnect the UI gets a `resync` so it reloads what it missed.
async fn gateway_loop(
    app: AppHandle,
    url: String,
    token: String,
    mut outgoing: mpsc::UnboundedReceiver<Value>,
) {
    let mut delay = Duration::from_secs(1);
    let mut first = true;
    loop {
        // Typing notices from while we were offline are stale.
        while outgoing.try_recv().is_ok() {}
        match gateway_session(&app, &url, &token, !first, &mut outgoing).await {
            Ok(()) => delay = Duration::from_secs(1),
            Err(e) => tracing::warn!("gateway: {e:#}"),
        }
        first = false;
        let _ = app.emit("gateway-status", json!({ "connected": false }));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn gateway_session(
    app: &AppHandle,
    url: &str,
    token: &str,
    resync: bool,
    outgoing: &mut mpsc::UnboundedReceiver<Value>,
) -> Result<()> {
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(ACCEPT_INVALID_CERTS)
        .build()?;
    let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        url,
        None,
        false,
        Some(tokio_tungstenite::Connector::NativeTls(tls)),
    )
    .await
    .with_context(|| format!("cannot connect to {url}"))?;
    ws.send(Message::text(
        json!({ "type": "auth", "token": token }).to_string(),
    ))
    .await?;
    loop {
        let msg = tokio::select! {
            msg = ws.next() => msg,
            out = outgoing.recv() => {
                match out {
                    Some(out) => ws.send(Message::text(out.to_string())).await?,
                    None => break,
                }
                continue;
            }
        };
        let Some(msg) = msg else { break };
        match msg? {
            Message::Text(text) => {
                let event: Value = serde_json::from_str(text.as_str())?;
                match event["type"].as_str() {
                    Some("ready") => {
                        tracing::info!("gateway connected");
                        let _ = app.emit("gateway-status", json!({ "connected": true }));
                        if resync {
                            let _ = app.emit("gateway", json!({ "type": "resync" }));
                        }
                    }
                    Some("error") => bail!("gateway refused: {}", event["error"]),
                    _ => {
                        let _ = app.emit("gateway", event);
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

/// A file name that does not exist yet in `dir` ("a.txt", "a (1).txt", ...).
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    // Never let a server-provided name escape the directory.
    let name = Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("download");
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("some file name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_server_addresses() {
        assert_eq!(normalize_server("localhost").unwrap(), "https://localhost");
        assert_eq!(
            normalize_server(" https://chat.example.org/ ").unwrap(),
            "https://chat.example.org"
        );
        assert_eq!(
            normalize_server("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert!(normalize_server("").is_err());
        assert!(normalize_server("ftp://example.org").is_err());
    }

    #[test]
    fn derives_gateway_url() {
        assert_eq!(
            gateway_url("https://localhost"),
            "wss://localhost/api/gateway"
        );
        assert_eq!(
            gateway_url("http://127.0.0.1:8080"),
            "ws://127.0.0.1:8080/api/gateway"
        );
    }

    #[test]
    fn extracts_error_messages() {
        assert_eq!(
            error_message(StatusCode::FORBIDDEN, r#"{"error":"missing permission"}"#),
            "missing permission"
        );
        assert_eq!(
            error_message(StatusCode::BAD_GATEWAY, "upstream down"),
            "upstream down"
        );
        assert_eq!(error_message(StatusCode::NOT_FOUND, ""), "404 Not Found");
    }

    #[test]
    fn saved_session_round_trip() {
        let dir = std::env::temp_dir().join(format!("tinlink-test-{}", std::process::id()));
        let path = dir.join("session.json");
        let saved = SavedSession {
            server: "https://localhost".into(),
            token: "secret".into(),
        };
        saved.save(&path).unwrap();
        assert_eq!(SavedSession::load(&path), Some(saved));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(SavedSession::load(&path), None);
    }

    #[test]
    fn unique_paths_do_not_overwrite_or_escape() {
        let dir = std::env::temp_dir().join(format!("tinlink-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (1).txt"));
        assert_eq!(unique_path(&dir, "../../evil.txt"), dir.join("evil.txt"));
        assert_eq!(unique_path(&dir, ".."), dir.join("download"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
