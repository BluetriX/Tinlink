//! Direct messages between two users.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn direct_messages_between_group_members(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let (stranger, _) = app.register("stranger").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;

    // Opening creates the channel once; both sides get the same one.
    let res = app
        .post("/api/dms", &alice, json!({ "user_id": owner_id }))
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    let dm = res.body["id"].as_str().unwrap().to_owned();
    assert_eq!(res.body["user"]["name"], "owner");
    let res = app
        .post("/api/dms", &alice, json!({ "user_id": owner_id }))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["id"], dm.as_str());
    let res = app
        .post("/api/dms", &owner, json!({ "user_id": alice_id }))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["id"], dm.as_str());

    // No shared group, or yourself: not possible.
    let res = app
        .post("/api/dms", &stranger, json!({ "user_id": owner_id }))
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app
        .post("/api/dms", &alice, json!({ "user_id": alice_id }))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let msgs = format!("/api/channels/{dm}/messages");
    let res = app
        .post(&msgs, &alice, json!({ "content": "hi @owner" }))
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    let msg = res.body["id"].as_str().unwrap().to_owned();

    // The other side sees one unread message until reading it.
    let list = app.get("/api/dms", &owner).await;
    assert_eq!(list.body[0]["id"], dm.as_str());
    assert_eq!(list.body[0]["user"]["name"], "alice");
    assert_eq!(list.body[0]["unread_count"], 1);
    assert_eq!(list.body[0]["last_message_id"], msg.as_str());
    let history = app.get(&msgs, &owner).await;
    assert_eq!(history.body[0]["content"], "hi @owner");
    let res = app
        .put(
            &format!("/api/channels/{dm}/read"),
            &owner,
            json!({ "message_id": msg }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let list = app.get("/api/dms", &owner).await;
    assert_eq!(list.body[0]["unread_count"], 0);
    // Your own messages never count as unread.
    let list = app.get("/api/dms", &alice).await;
    assert_eq!(list.body[0]["unread_count"], 0);

    // Outsiders see nothing.
    assert_eq!(
        app.get(&msgs, &stranger).await.status,
        StatusCode::NOT_FOUND
    );
    let res = app.post(&msgs, &stranger, json!({ "content": "x" })).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    assert_eq!(app.get("/api/dms", &stranger).await.body, json!([]));

    // Only the author edits or deletes; there are no moderators.
    let res = app.delete(&format!("{msgs}/{msg}"), &owner).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app
        .patch(
            &format!("{msgs}/{msg}"),
            &alice,
            json!({ "content": "edited" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = app.delete(&format!("{msgs}/{msg}"), &alice).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    // Direct message channels are not group channels.
    let res = app
        .post(
            &format!("/api/channels/{dm}/voice-token"),
            &owner,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app
        .patch(
            &format!("/api/channels/{dm}"),
            &owner,
            json!({ "name": "x" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn conversations_stay_open_after_leaving_the_group(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let res = app
        .post("/api/dms", &alice, json!({ "user_id": owner_id }))
        .await;
    let dm = res.body["id"].as_str().unwrap().to_owned();

    let res = app
        .delete(&format!("/api/groups/{group}/members/{alice_id}"), &alice)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = app
        .post("/api/dms", &owner, json!({ "user_id": alice_id }))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["id"], dm.as_str());
    let res = app
        .post(
            &format!("/api/channels/{dm}/messages"),
            &owner,
            json!({ "content": "still here" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
}
