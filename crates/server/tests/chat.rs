//! Read markers, mentions and password changes.

mod common;

use axum::http::{Method, StatusCode};
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

fn channel<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn read_markers_and_mentions(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let (bob, _) = app.register("bob").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    let msgs = format!("/api/channels/{general}/messages");

    // Nothing posted yet.
    let state = app.channels(&alice, &group).await;
    let c = channel(&state, &general);
    assert_eq!(c["last_message_id"], Value::Null);
    assert_eq!(c["mention_count"], 0);

    // Mentions resolve to group members other than the author; bob is no
    // member and e-mail addresses do not count.
    let res = app
        .post(
            &msgs,
            &owner,
            json!({ "content": "@Alice @owner @bob mail a@alice.org" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(res.body["mentions"], json!([alice_id]));
    let first = res.body["id"].as_str().unwrap().to_owned();
    let res = app
        .post(&msgs, &owner, json!({ "content": "no mention" }))
        .await;
    let second = res.body["id"].as_str().unwrap().to_owned();

    let state = app.channels(&alice, &group).await;
    let c = channel(&state, &general);
    assert_eq!(c["last_message_id"], second);
    assert_eq!(c["last_read_id"], Value::Null);
    assert_eq!(c["mention_count"], 1);
    // The author has read their own messages.
    let state = app.channels(&owner, &group).await;
    assert_eq!(channel(&state, &general)["last_read_id"], second);

    // History includes mentions.
    let history = app.get(&msgs, &alice).await.body;
    assert_eq!(history[1]["mentions"], json!([alice_id]));
    assert_eq!(history[0]["mentions"], json!([]));

    let read = format!("/api/channels/{general}/read");
    let res = app.put(&read, &alice, json!({ "message_id": first })).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let state = app.channels(&alice, &group).await;
    let c = channel(&state, &general);
    assert_eq!(c["last_read_id"], first);
    assert_eq!(c["mention_count"], 0);

    // The marker never moves back; unknown messages and outsiders are rejected.
    app.put(&read, &alice, json!({ "message_id": second }))
        .await;
    app.put(&read, &alice, json!({ "message_id": first })).await;
    let state = app.channels(&alice, &group).await;
    assert_eq!(channel(&state, &general)["last_read_id"], second);
    let res = app
        .put(&read, &alice, json!({ "message_id": uuid::Uuid::now_v7() }))
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app.put(&read, &bob, json!({ "message_id": second })).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Editing updates mentions.
    let res = app
        .patch(
            &format!("{msgs}/{second}"),
            &owner,
            json!({ "content": "now @alice" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(res.body["mentions"], json!([alice_id]));
    let res = app
        .patch(
            &format!("{msgs}/{first}"),
            &owner,
            json!({ "content": "gone" }),
        )
        .await;
    assert_eq!(res.body["mentions"], json!([]));
    let _ = owner_id;
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn members_who_cannot_see_the_channel_are_not_mentioned(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({ "name": "staff" }),
        )
        .await;
    let staff = res.body["id"].as_str().unwrap().to_owned();
    app.put(
        &format!("/api/channels/{staff}/overwrites/role/{group}"),
        &owner,
        json!({ "deny": "VIEW_CHANNEL" }),
    )
    .await;
    let res = app
        .post(
            &format!("/api/channels/{staff}/messages"),
            &owner,
            json!({ "content": "hey @alice" }),
        )
        .await;
    assert_eq!(res.body["mentions"], json!([]));
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn change_password_signs_out_other_sessions(db: PgPool) {
    let app = TestApp::new(db);
    let (token, _) = app.register("alice").await;
    let login = |password: &str| {
        app.request(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({ "name": "alice", "password": password })),
        )
    };
    let other = login("correct horse battery").await.body["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let res = app
        .put(
            "/api/users/me/password",
            &token,
            json!({ "current_password": "wrong", "new_password": "new password 123" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = app
        .put(
            "/api/users/me/password",
            &token,
            json!({ "current_password": "correct horse battery", "new_password": "short" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = app
        .put(
            "/api/users/me/password",
            &token,
            json!({ "current_password": "correct horse battery", "new_password": "new password 123" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    assert_eq!(
        app.get("/api/users/me", &token).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.get("/api/users/me", &other).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login("correct horse battery").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(login("new password 123").await.status, StatusCode::OK);
}

async fn my_group(app: &TestApp, token: &str) -> Value {
    app.get("/api/groups", token).await.body[0].clone()
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn group_list_shows_unread_and_mentions(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    assert_eq!(my_group(&app, &alice).await["unread"], false);

    let res = app
        .post(
            &format!("/api/channels/{general}/messages"),
            &owner,
            json!({ "content": "hi @alice" }),
        )
        .await;
    let id = res.body["id"].as_str().unwrap().to_owned();
    let g = my_group(&app, &alice).await;
    assert_eq!(g["id"], group);
    assert_eq!(g["unread"], true);
    assert_eq!(g["mention_count"], 1);
    assert_eq!(my_group(&app, &owner).await["unread"], false);

    // Hidden channels do not count.
    app.put(
        &format!("/api/channels/{general}/overwrites/role/{group}"),
        &owner,
        json!({ "deny": "VIEW_CHANNEL" }),
    )
    .await;
    assert_eq!(my_group(&app, &alice).await["unread"], false);
    app.delete(
        &format!("/api/channels/{general}/overwrites/role/{group}"),
        &owner,
    )
    .await;

    app.put(
        &format!("/api/channels/{general}/read"),
        &alice,
        json!({ "message_id": id }),
    )
    .await;
    let g = my_group(&app, &alice).await;
    assert_eq!(g["unread"], false);
    assert_eq!(g["mention_count"], 0);
}
