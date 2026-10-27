//! Own profile (display name, avatar) and group/channel icons.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

fn data_url(bytes: &[u8]) -> String {
    format!("data:image/png;base64,{}", STANDARD.encode(bytes))
}

async fn fetch_image(app: &TestApp, id: &Value) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/images/{}", id.as_str().unwrap()))
        .body(Body::empty())
        .unwrap();
    app.raw(req).await
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn edit_own_profile(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;

    let res = app
        .patch(
            "/api/users/me",
            &owner,
            json!({ "display_name": "  Big Boss " }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(res.body["display_name"], "Big Boss");
    assert_eq!(res.body["name"], "owner");
    let res = app
        .patch("/api/users/me", &owner, json!({ "display_name": " " }))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = app
        .put(
            "/api/users/me/avatar",
            &owner,
            json!({ "image": data_url(PNG) }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    let avatar = res.body["avatar_id"].clone();
    assert!(avatar.is_string());

    // Other members see name and avatar; the image needs no login.
    let members = app
        .get(&format!("/api/groups/{group}/members"), &alice)
        .await;
    let me = members
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["user_id"] == owner_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(me["display_name"], "Big Boss");
    assert_eq!(me["avatar_id"], avatar);
    let (status, headers, body) = fetch_image(&app, &avatar).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(body, PNG);

    // Replacing the avatar deletes the old image.
    let res = app
        .put(
            "/api/users/me/avatar",
            &owner,
            json!({ "image": data_url(PNG) }),
        )
        .await;
    assert_ne!(res.body["avatar_id"], avatar);
    assert_eq!(fetch_image(&app, &avatar).await.0, StatusCode::NOT_FOUND);
    let res = app
        .put("/api/users/me/avatar", &owner, json!({ "image": null }))
        .await;
    assert_eq!(res.body["avatar_id"], Value::Null);
    assert_eq!(
        app.get("/api/users/me", &owner).await.body["avatar_id"],
        Value::Null
    );

    // Scripts disguised as images are rejected.
    let res = app
        .put(
            "/api/users/me/avatar",
            &owner,
            json!({ "image": data_url(b"<svg onload=alert(1)>") }),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn group_and_channel_icons(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let icon = json!({ "image": data_url(PNG) });
    let path = format!("/api/groups/{group}/icon");
    assert_eq!(
        app.put(&path, &alice, icon.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    let res = app.put(&path, &owner, icon.clone()).await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    let group_icon = res.body["icon_id"].clone();
    let groups = app.get("/api/groups", &alice).await;
    assert_eq!(groups.body[0]["icon_id"], group_icon);

    let path = format!("/api/channels/{general}/icon");
    assert_eq!(
        app.put(&path, &alice, icon.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    let res = app.put(&path, &owner, icon).await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert!(res.body["icon_id"].is_string());

    // Emoji: set, keep when missing, clear with null.
    let path = format!("/api/channels/{general}");
    let res = app.patch(&path, &owner, json!({ "emoji": "🎮" })).await;
    assert_eq!(res.body["emoji"], "🎮");
    let res = app.patch(&path, &owner, json!({ "name": "games" })).await;
    assert_eq!(res.body["emoji"], "🎮");
    let res = app.patch(&path, &owner, json!({ "emoji": null })).await;
    assert_eq!(res.body["emoji"], Value::Null);
    let res = app
        .patch(&path, &owner, json!({ "emoji": "x".repeat(20) }))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Icons of a deleted group are swept.
    app.delete(&format!("/api/groups/{group}"), &owner).await;
    tinlink_server::tasks::sweep(
        &app.state,
        std::time::Duration::ZERO,
        std::time::Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(
        fetch_image(&app, &group_icon).await.0,
        StatusCode::NOT_FOUND
    );
}
