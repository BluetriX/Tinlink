//! LiveKit webhooks: voice presence and temporary channel cleanup.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD as B64};
use common::{LIVEKIT_SECRET, TestApp};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Signs a webhook body the way LiveKit does.
fn webhook_auth(body: &str, secret: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let header = B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = json!({
        "iss": "testkey", "nbf": now, "exp": now + 300,
        "sha256": STANDARD.encode(Sha256::digest(body.as_bytes())),
    });
    let input = format!("{header}.{}", B64.encode(claims.to_string()));
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(input.as_bytes());
    format!("{input}.{}", B64.encode(mac.finalize().into_bytes()))
}

async fn send_webhook(app: &TestApp, body: Value) -> StatusCode {
    let body = body.to_string();
    send_raw(app, &body, &webhook_auth(&body, LIVEKIT_SECRET)).await
}

async fn send_raw(app: &TestApp, body: &str, auth: &str) -> StatusCode {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/livekit/webhook")
        .header("authorization", auth)
        .header("content-type", "application/webhook+json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    tinlink_server::router(app.state.clone())
        .oneshot(req)
        .await
        .unwrap()
        .status()
}

fn participant_event(event: &str, room: &str, identity: &str) -> Value {
    json!({ "event": event, "room": { "name": room, "sid": "RM_x" }, "participant": { "identity": identity, "sid": "PA_x" } })
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn webhooks_track_voice_presence(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let status = send_webhook(
        &app,
        participant_event("participant_joined", &general, "alice@test.local"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Repeated delivery is harmless.
    send_webhook(
        &app,
        participant_event("participant_joined", &general, "alice@test.local"),
    )
    .await;
    // Unknown users, foreign domains and non-channel rooms are ignored.
    send_webhook(
        &app,
        participant_event("participant_joined", &general, "ghost@test.local"),
    )
    .await;
    send_webhook(
        &app,
        participant_event("participant_joined", &general, "alice@elsewhere.org"),
    )
    .await;
    assert_eq!(
        send_webhook(
            &app,
            participant_event("participant_joined", "lobby", "alice@test.local")
        )
        .await,
        StatusCode::OK
    );

    let voice = app.channels(&owner, &group).await["voice"].clone();
    assert_eq!(
        voice,
        json!([{ "channel_id": general, "user_id": alice_id }])
    );

    send_webhook(
        &app,
        participant_event("participant_left", &general, "alice@test.local"),
    )
    .await;
    assert_eq!(app.channels(&owner, &group).await["voice"], json!([]));
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn rejects_unsigned_or_tampered_webhooks(db: PgPool) {
    let app = TestApp::new(db);
    let body = json!({"event": "room_finished", "room": {"name": "x"}}).to_string();
    assert_eq!(
        send_raw(&app, &body, "nonsense").await,
        StatusCode::UNAUTHORIZED
    );
    let auth = webhook_auth(&body, "wrong-secret-wrong-secret-wrong-secret");
    assert_eq!(send_raw(&app, &body, &auth).await, StatusCode::UNAUTHORIZED);
    let auth = webhook_auth(&body, LIVEKIT_SECRET);
    assert_eq!(
        send_raw(&app, &body.replace("finished", "started"), &auth).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(send_raw(&app, &body, &auth).await, StatusCode::OK);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn temporary_channels_are_removed_when_empty(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let rooms = app.category_id(&owner, &group, "Rooms").await;

    let create = |name: &'static str| {
        let path = format!("/api/groups/{group}/channels");
        let body = json!({ "name": name, "category_id": rooms, "temporary": true });
        let alice = alice.clone();
        let app = &app;
        async move {
            app.post(&path, &alice, body).await.body["id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let used = create("used").await;
    let unused = create("unused").await;

    // The used room finishes: its channel is deleted by the webhook.
    send_webhook(
        &app,
        participant_event("participant_joined", &used, "alice@test.local"),
    )
    .await;
    send_webhook(
        &app,
        json!({ "event": "room_finished", "room": { "name": used } }),
    )
    .await;
    assert!(app.channel_id(&owner, &group, "used").await.is_none());

    // Regular channels survive room_finished.
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    send_webhook(
        &app,
        json!({ "event": "room_finished", "room": { "name": general } }),
    )
    .await;
    assert!(app.channel_id(&owner, &group, "general").await.is_some());

    // The never-used room is swept once it is old enough.
    tinlink_server::tasks::sweep(
        &app.state,
        Duration::from_secs(3600),
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    assert!(app.channel_id(&owner, &group, "unused").await.is_some());
    tinlink_server::tasks::sweep(&app.state, Duration::ZERO, Duration::from_secs(3600))
        .await
        .unwrap();
    assert!(app.channel_id(&owner, &group, &unused).await.is_none());
    assert!(app.channel_id(&owner, &group, "unused").await.is_none());
}
