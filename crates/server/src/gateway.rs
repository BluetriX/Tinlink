//! WebSocket gateway: pushes live events to connected clients.
//!
//! Protocol (JSON text frames):
//! 1. client → `{"type":"auth","token":"<session token>"}` within 10 s
//! 2. server → `{"type":"ready","user_id":"..."}`
//! 3. server → events (see `events::Event`), or `{"type":"resync"}` when the
//!    connection fell behind and the client should refetch its state.
//!
//! The token is sent as a message instead of a query parameter so that it
//! does not end up in proxy or access logs.

use std::collections::HashSet;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::access::channel_access;
use crate::auth::{self, AuthUser};
use crate::events::{Event, EventKind};
use crate::state::AppState;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);

pub async fn upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(err) = run(state, socket).await {
            tracing::debug!(error = %err, "gateway connection closed");
        }
    })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Auth { token: String },
}

async fn run(state: AppState, socket: WebSocket) -> anyhow::Result<()> {
    let (mut sink, mut stream) = socket.split();

    let user = match tokio::time::timeout(AUTH_TIMEOUT, read_auth(&state, &mut stream)).await {
        Ok(Some(user)) => user,
        _ => {
            let _ = sink
                .send(text(
                    json!({"type": "error", "error": "authentication failed"}),
                ))
                .await;
            return Ok(());
        }
    };
    // Subscribe before loading memberships so no event falls in between.
    let mut events = state.events.subscribe();
    let mut groups: HashSet<Uuid> =
        sqlx::query_scalar("SELECT group_id FROM members WHERE user_id = $1")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect();
    sink.send(text(json!({"type": "ready", "user_id": user.id})))
        .await?;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                // Clients have nothing to say after auth; pongs are handled by axum.
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
            },
            event = events.recv() => match event {
                Ok(event) => {
                    if should_deliver(&state, &user, &mut groups, &event).await {
                        sink.send(text(serde_json::to_value(&*event)?)).await?;
                    }
                }
                Err(RecvError::Lagged(_)) => sink.send(text(json!({"type": "resync"}))).await?,
                Err(RecvError::Closed) => return Ok(()),
            },
            _ = ping.tick() => sink.send(Message::Ping(Default::default())).await?,
        }
    }
}

async fn read_auth(
    state: &AppState,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Option<AuthUser> {
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Text(body) = msg {
            let ClientMessage::Auth { token } = serde_json::from_str(&body).ok()?;
            return auth::authenticate(&state.db, &token).await.ok().flatten();
        }
    }
    None
}

/// Decides whether `user` may see `event`, keeping the set of joined
/// groups up to date as membership events pass by.
async fn should_deliver(
    state: &AppState,
    user: &AuthUser,
    groups: &mut HashSet<Uuid>,
    event: &Event,
) -> bool {
    let is_self = event.user_id == Some(user.id);
    match event.kind {
        EventKind::MemberJoined if is_self => {
            groups.insert(event.group_id);
            return true;
        }
        EventKind::MemberLeft if is_self => {
            return groups.remove(&event.group_id);
        }
        EventKind::GroupDeleted => return groups.remove(&event.group_id),
        _ => {}
    }
    if !groups.contains(&event.group_id) {
        return false;
    }
    match event.channel_id {
        Some(channel_id) => channel_access(&state.db, channel_id, user.id).await.is_ok(),
        None => true,
    }
}

fn text(value: serde_json::Value) -> Message {
    Message::Text(Utf8Bytes::from(value.to_string()))
}
