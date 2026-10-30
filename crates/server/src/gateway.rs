//! WebSocket gateway: pushes live events to connected clients.
//!
//! Protocol (JSON text frames):
//! 1. client → `{"type":"auth","token":"<session token>"}` within 10 s
//! 2. server → `{"type":"ready","user_id":"..."}`
//! 3. server → events (see `events::Event`), or `{"type":"resync"}` when the
//!    connection fell behind and the client should refetch its state.
//! 4. client → `{"type":"typing","channel_id":"..."}` while the user types,
//!    at most every few seconds; the server relays it as a `typing` event.
//!
//! A user with at least one open connection is online; coming online and
//! going offline is announced to their groups as `presence_changed`.
//! The token is sent as a message instead of a query parameter so that it
//! does not end up in proxy or access logs.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tinlink_protocol::Permissions;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use crate::access::{channel_access, chat_access, is_direct_participant};
use crate::auth::{self, AuthUser};
use crate::events::{Event, EventKind};
use crate::state::AppState;

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Typing notices per channel and connection are relayed at most this often.
const TYPING_INTERVAL: Duration = Duration::from_secs(2);

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
    Typing { channel_id: Uuid },
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
    let events = state.events.subscribe();
    let mut groups: HashSet<Uuid> =
        sqlx::query_scalar("SELECT group_id FROM members WHERE user_id = $1")
            .bind(user.id)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .collect();
    sink.send(text(json!({"type": "ready", "user_id": user.id})))
        .await?;

    if state.presence.connect(user.id) {
        announce_presence(&state, &user, &groups, true);
    }
    let result = serve(&state, &user, &mut groups, events, &mut sink, &mut stream).await;
    if state.presence.disconnect(user.id) {
        announce_presence(&state, &user, &groups, false);
    }
    result
}

type Sink = futures_util::stream::SplitSink<WebSocket, Message>;
type Stream = futures_util::stream::SplitStream<WebSocket>;

async fn serve(
    state: &AppState,
    user: &AuthUser,
    groups: &mut HashSet<Uuid>,
    mut events: tokio::sync::broadcast::Receiver<std::sync::Arc<Event>>,
    sink: &mut Sink,
    stream: &mut Stream,
) -> anyhow::Result<()> {
    let mut last_typing: HashMap<Uuid, Instant> = HashMap::new();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(Message::Text(body))) => {
                    if let Ok(ClientMessage::Typing { channel_id }) = serde_json::from_str(&body) {
                        let now = Instant::now();
                        let recent = last_typing
                            .get(&channel_id)
                            .is_some_and(|t| now.duration_since(*t) < TYPING_INTERVAL);
                        if !recent {
                            last_typing.insert(channel_id, now);
                            relay_typing(state, user, channel_id).await;
                        }
                    }
                }
                // Pongs are handled by axum.
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
            },
            event = events.recv() => match event {
                Ok(event) => {
                    if should_deliver(state, user, groups, &event).await {
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

async fn read_auth(state: &AppState, stream: &mut Stream) -> Option<AuthUser> {
    while let Some(Ok(msg)) = stream.next().await {
        if let Message::Text(body) = msg {
            let ClientMessage::Auth { token } = serde_json::from_str(&body).ok()? else {
                return None;
            };
            return auth::authenticate(&state.db, &token).await.ok().flatten();
        }
    }
    None
}

fn announce_presence(state: &AppState, user: &AuthUser, groups: &HashSet<Uuid>, online: bool) {
    for group_id in groups {
        state.events.publish(Event {
            kind: EventKind::PresenceChanged,
            group_id: Some(*group_id),
            channel_id: None,
            user_id: Some(user.id),
            data: json!({ "online": online }),
        });
    }
}

/// Relays a typing notice if the user may write in the channel.
async fn relay_typing(state: &AppState, user: &AuthUser, channel_id: Uuid) {
    let Ok(access) = chat_access(&state.db, channel_id, user.id).await else {
        return;
    };
    if access.perms.contains(Permissions::SEND_MESSAGES) {
        state.events.publish(Event {
            kind: EventKind::Typing,
            group_id: access.group_id,
            channel_id: Some(channel_id),
            user_id: Some(user.id),
            data: json!({ "name": user.name }),
        });
    }
}

/// Decides whether `user` may see `event`, keeping the set of joined
/// groups up to date as membership events pass by.
async fn should_deliver(
    state: &AppState,
    user: &AuthUser,
    groups: &mut HashSet<Uuid>,
    event: &Event,
) -> bool {
    if matches!(event.kind, EventKind::UserUpdated | EventKind::KeysChanged) {
        return true;
    }
    let Some(group_id) = event.group_id else {
        // Direct message channels: only the two participants.
        return match event.channel_id {
            Some(channel_id) => is_direct_participant(&state.db, channel_id, user.id).await,
            None => false,
        };
    };
    let is_self = event.user_id == Some(user.id);
    match event.kind {
        EventKind::MemberJoined if is_self => {
            groups.insert(group_id);
            return true;
        }
        EventKind::MemberLeft if is_self => {
            return groups.remove(&group_id);
        }
        EventKind::GroupDeleted => return groups.remove(&group_id),
        _ => {}
    }
    if !groups.contains(&group_id) {
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
