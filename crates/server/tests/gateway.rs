//! WebSocket gateway tests: authentication and permission-filtered events.

mod common;

use std::time::Duration;

use common::TestApp;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(addr: std::net::SocketAddr, token: &str) -> Ws {
    let (mut ws, _) = connect_async(format!("ws://{addr}/api/gateway"))
        .await
        .unwrap();
    ws.send(Message::text(
        json!({"type": "auth", "token": token}).to_string(),
    ))
    .await
    .unwrap();
    let ready = next_json(&mut ws).await.expect("ready message");
    assert_eq!(ready["type"], "ready", "{ready}");
    ws
}

/// Next JSON message, or None if nothing arrives within a short time.
async fn next_json(ws: &mut Ws) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(serde_json::from_str(&t).unwrap()),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn rejects_invalid_token(db: PgPool) {
    let app = TestApp::new(db);
    let addr = app.spawn().await;
    let (mut ws, _) = connect_async(format!("ws://{addr}/api/gateway"))
        .await
        .unwrap();
    ws.send(Message::text(
        json!({"type": "auth", "token": "nope"}).to_string(),
    ))
    .await
    .unwrap();
    let msg = next_json(&mut ws).await.unwrap();
    assert_eq!(msg["type"], "error");
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn owner_receives_events_of_group_created_while_connected(db: PgPool) {
    let app = TestApp::new(db);
    let addr = app.spawn().await;
    let (owner, _) = app.register("owner").await;
    let mut ws = connect(addr, &owner).await;

    let group = app.create_group(&owner, "G").await;
    let joined = next_json(&mut ws)
        .await
        .expect("member_joined for the owner");
    assert_eq!(joined["type"], "member_joined");
    assert_eq!(joined["group_id"], group.as_str());

    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    app.post(
        &format!("/api/channels/{general}/messages"),
        &owner,
        json!({"content": "hello"}),
    )
    .await;
    let event = next_json(&mut ws).await.expect("message_created");
    assert_eq!(event["type"], "message_created");
    assert_eq!(event["data"]["content"], "hello");
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn delivers_events_only_to_users_with_access(db: PgPool) {
    let app = TestApp::new(db);
    let addr = app.spawn().await;
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let (stranger, _) = app.register("stranger").await;
    let group = app.create_group(&owner, "G").await;

    let mut alice_ws = connect(addr, &alice).await;
    let mut stranger_ws = connect(addr, &stranger).await;

    // Alice joins: she receives her own member_joined and from then on group events.
    app.invite_and_join(&owner, &group, &alice).await;
    let joined = next_json(&mut alice_ws).await.unwrap();
    assert_eq!(joined["type"], "member_joined");
    assert_eq!(joined["group_id"], group.as_str());

    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    app.post(
        &format!("/api/channels/{general}/messages"),
        &owner,
        json!({"content": "hello"}),
    )
    .await;
    let created = next_json(&mut alice_ws).await.unwrap();
    assert_eq!(created["type"], "message_created");
    assert_eq!(created["channel_id"], general.as_str());
    assert_eq!(created["data"]["content"], "hello");

    // A hidden channel: alice gets the group_updated notice but no messages from it.
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({"name": "secret"}),
        )
        .await;
    let secret = res.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        next_json(&mut alice_ws).await.unwrap()["type"],
        "channel_created"
    );
    app.put(
        &format!("/api/channels/{secret}/overwrites/role/{group}"),
        &owner,
        json!({"deny": "VIEW_CHANNEL"}),
    )
    .await;
    assert_eq!(
        next_json(&mut alice_ws).await.unwrap()["type"],
        "group_updated"
    );
    app.post(
        &format!("/api/channels/{secret}/messages"),
        &owner,
        json!({"content": "psst"}),
    )
    .await;
    assert!(
        next_json(&mut alice_ws).await.is_none(),
        "hidden message leaked"
    );

    // Strangers never see anything from the group.
    assert!(next_json(&mut stranger_ws).await.is_none());

    // After leaving, alice gets member_left and nothing more.
    let alice_id = app.get("/api/users/me", &alice).await.body["id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.delete(&format!("/api/groups/{group}/members/{alice_id}"), &alice)
        .await;
    assert_eq!(
        next_json(&mut alice_ws).await.unwrap()["type"],
        "member_left"
    );
    app.post(
        &format!("/api/channels/{general}/messages"),
        &owner,
        json!({"content": "bye"}),
    )
    .await;
    assert!(next_json(&mut alice_ws).await.is_none());
}
