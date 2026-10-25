//! Voice moderation: server-side mute, disconnect and move, checked against
//! a mock of the LiveKit RoomService API.

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Serves `/twirp/livekit.RoomService/{method}` and records every call.
async fn mock_livekit() -> (String, Calls) {
    async fn handle(
        State(calls): State<Calls>,
        Path(method): Path<String>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, String) {
        if !headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.starts_with("Bearer "))
        {
            return (StatusCode::UNAUTHORIZED, "{}".into());
        }
        let body: Value = serde_json::from_str(&body).unwrap();
        calls.lock().unwrap().push((method, body));
        (StatusCode::OK, "{}".into())
    }

    let calls = Calls::default();
    let app = Router::new()
        .route("/twirp/livekit.RoomService/{method}", post(handle))
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}

async fn app_with_mock(db: PgPool) -> (TestApp, Calls) {
    let (url, calls) = mock_livekit().await;
    let mut config = common::config();
    config.livekit.internal_url = Some(url);
    (TestApp::with_config(db, config), calls)
}

/// Marks a user as connected to a channel's voice room, as the webhook would.
async fn connect(app: &TestApp, channel: &str, user: &str) {
    sqlx::query("INSERT INTO voice_participants (channel_id, user_id) VALUES ($1::uuid, $2::uuid)")
        .bind(channel)
        .bind(user)
        .execute(&app.state.db)
        .await
        .unwrap();
}

async fn create_role(app: &TestApp, owner: &str, group: &str, user: &str, perms: &str, pos: i32) {
    let res = app
        .post(
            &format!("/api/groups/{group}/roles"),
            owner,
            json!({ "name": format!("r{pos}"), "position": pos, "permissions": perms }),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    let role = res.body["id"].as_str().unwrap();
    let res = app
        .put(
            &format!("/api/groups/{group}/members/{user}/roles/{role}"),
            owner,
            json!({}),
        )
        .await;
    assert!(res.status.is_success(), "{:?}", res.body);
}

/// Returns the publish sources granted by a voice token.
async fn token_sources(app: &TestApp, channel: &str, token: &str) -> Value {
    let res = app
        .post(
            &format!("/api/channels/{channel}/voice-token"),
            token,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    let jwt = res.body["token"].as_str().unwrap();
    let claims: Value =
        serde_json::from_slice(&B64.decode(jwt.split('.').nth(1).unwrap()).unwrap()).unwrap();
    claims["video"]["canPublishSources"].clone()
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn server_mute_updates_tokens_and_live_participants(db: PgPool) {
    let (app, calls) = app_with_mock(db).await;
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    assert!(
        token_sources(&app, &general, &alice)
            .await
            .as_array()
            .unwrap()
            .contains(&json!("microphone"))
    );
    connect(&app, &general, &alice_id).await;

    let mute_path = format!("/api/groups/{group}/members/{alice_id}/voice-mute");
    let res = app.put(&mute_path, &owner, json!({ "muted": true })).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{:?}", res.body);

    // The connected participant lost the microphone right away ...
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (method, body) = &calls[0];
        assert_eq!(method, "UpdateParticipant");
        assert_eq!(body["room"], general);
        assert_eq!(body["identity"], "alice@test.local");
        let sources = body["permission"]["canPublishSources"].as_array().unwrap();
        assert!(!sources.contains(&json!("MICROPHONE")));
        assert!(sources.contains(&json!("SCREEN_SHARE")));
    }
    // ... and new tokens do not grant it either.
    assert!(
        !token_sources(&app, &general, &alice)
            .await
            .as_array()
            .unwrap()
            .contains(&json!("microphone"))
    );

    // Members cannot mute themselves out of it or mute others.
    let res = app.put(&mute_path, &alice, json!({ "muted": false })).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    let res = app.put(&mute_path, &owner, json!({ "muted": false })).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].1["permission"]["canPublishSources"]
            .as_array()
            .unwrap()
            .contains(&json!("MICROPHONE"))
    );
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn moderators_need_a_higher_role(db: PgPool) {
    let (app, calls) = app_with_mock(db).await;
    let (owner, owner_id) = app.register("owner").await;
    let (moder, mod_id) = app.register("moder").await;
    let (alice, alice_id) = app.register("alice").await;
    let (bob, bob_id) = app.register("bob").await;
    let group = app.create_group(&owner, "G").await;
    for t in [&moder, &alice, &bob] {
        app.invite_and_join(&owner, &group, t).await;
    }
    create_role(
        &app,
        &owner,
        &group,
        &mod_id,
        "MUTE_MEMBERS | MOVE_MEMBERS",
        10,
    )
    .await;
    create_role(&app, &owner, &group, &bob_id, "VIEW_CHANNEL", 20).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let mute = |user: &str| format!("/api/groups/{group}/members/{user}/voice-mute");
    let kick = |user: &str| format!("/api/channels/{general}/voice/{user}/disconnect");

    // A plain member may do neither.
    let res = app
        .put(&mute(&alice_id), &bob, json!({ "muted": true }))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app.post(&kick(&alice_id), &bob, json!({})).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The moderator outranks alice but neither bob (higher role) nor the owner.
    let res = app
        .put(&mute(&alice_id), &moder, json!({ "muted": true }))
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    for target in [&bob_id, &owner_id] {
        let res = app
            .put(&mute(target), &moder, json!({ "muted": true }))
            .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
        let res = app.post(&kick(target), &moder, json!({})).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
    }

    let res = app.post(&kick(&alice_id), &moder, json!({})).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    // Anyone may disconnect themselves.
    let res = app.post(&kick(&bob_id), &bob, json!({})).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    let calls = calls.lock().unwrap();
    let removed: Vec<_> = calls
        .iter()
        .filter(|(m, _)| m == "RemoveParticipant")
        .map(|(_, b)| b["identity"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(removed, ["alice@test.local", "bob@test.local"]);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn move_disconnects_and_announces_target(db: PgPool) {
    let (app, calls) = app_with_mock(db).await;
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    let other_group = app.create_group(&owner, "Other").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({ "name": "afk" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    let afk = res.body["id"].as_str().unwrap().to_owned();
    let foreign = app
        .channel_id(&owner, &other_group, "general")
        .await
        .unwrap();

    let mut events = app.state.events.subscribe();
    let path = format!("/api/channels/{general}/voice/{alice_id}/move");

    // Members cannot move others; channels of other groups are rejected.
    let res = app.post(&path, &alice, json!({ "to": afk })).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app.post(&path, &owner, json!({ "to": foreign })).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = app.post(&path, &owner, json!({ "to": afk })).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{:?}", res.body);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [(
            "RemoveParticipant".to_owned(),
            json!({ "room": general, "identity": "alice@test.local" })
        )]
    );
    let event = loop {
        let event = serde_json::to_value(events.recv().await.unwrap()).unwrap();
        if event["type"] == "voice_moved" {
            break event;
        }
    };
    assert_eq!(event["user_id"], alice_id);
    assert_eq!(event["data"], json!({ "from": general, "to": afk }));
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn losing_access_disconnects_participants(db: PgPool) {
    let (app, calls) = app_with_mock(db).await;
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    connect(&app, &general, &alice_id).await;

    // Hide the channel from @everyone (role id = group id), then sync.
    let res = app
        .put(
            &format!("/api/channels/{general}/overwrites/role/{group}"),
            &owner,
            json!({ "deny": "VIEW_CHANNEL" }),
        )
        .await;
    assert!(res.status.is_success(), "{:?}", res.body);
    tinlink_server::voice_sync::refresh_group(&app.state, group.parse().unwrap(), None)
        .await
        .unwrap();

    let calls = calls.lock().unwrap();
    assert!(calls.contains(&(
        "RemoveParticipant".to_owned(),
        json!({ "room": general, "identity": "alice@test.local" })
    )));
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn moderation_without_livekit_api_is_reported(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    // Muting still works (it applies to new tokens), kicking needs the API.
    let res = app
        .put(
            &format!("/api/groups/{group}/members/{alice_id}/voice-mute"),
            &owner,
            json!({ "muted": true }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = app
        .post(
            &format!("/api/channels/{general}/voice/{alice_id}/disconnect"),
            &owner,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}
