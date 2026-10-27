//! Group administration: categories, moving channels, invites.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn categories_and_moving_channels(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let res = app
        .post(
            &format!("/api/groups/{group}/categories"),
            &owner,
            json!({ "name": "Games" }),
        )
        .await;
    let games = res.body["id"].as_str().unwrap().to_owned();
    let res = app
        .patch(
            &format!("/api/categories/{games}"),
            &owner,
            json!({ "name": "Gaming", "position": 5, "allow_user_channels": true }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(res.body["name"], "Gaming");
    assert_eq!(res.body["allow_user_channels"], true);

    // Move a channel in, out (null) and leave it alone when the field is missing.
    let path = format!("/api/channels/{general}");
    let res = app
        .patch(&path, &owner, json!({ "category_id": games }))
        .await;
    assert_eq!(res.body["category_id"], games.as_str());
    let res = app.patch(&path, &owner, json!({ "name": "lobby" })).await;
    assert_eq!(res.body["category_id"], games.as_str());
    let res = app
        .patch(&path, &owner, json!({ "category_id": null }))
        .await;
    assert_eq!(res.body["category_id"], Value::Null);

    // Categories of other groups are rejected.
    let other = app.create_group(&owner, "Other").await;
    let res = app
        .post(
            &format!("/api/groups/{other}/categories"),
            &owner,
            json!({ "name": "Foreign" }),
        )
        .await;
    let foreign = res.body["id"].as_str().unwrap().to_owned();
    let res = app
        .patch(&path, &owner, json!({ "category_id": foreign }))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Members without MANAGE_CHANNELS cannot touch categories.
    let res = app
        .patch(
            &format!("/api/categories/{games}"),
            &alice,
            json!({ "name": "x" }),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.delete(&format!("/api/categories/{games}"), &alice)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Deleting a category keeps its channels.
    app.patch(&path, &owner, json!({ "category_id": games }))
        .await;
    let res = app
        .delete(&format!("/api/categories/{games}"), &owner)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let list = app.channels(&owner, &group).await;
    let ch = list["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == general.as_str())
        .unwrap();
    assert_eq!(ch["category_id"], Value::Null);

    // The group detail tells clients how high the caller ranks.
    let detail = app.get(&format!("/api/groups/{group}"), &alice).await;
    assert_eq!(detail.body["my_position"], 0);
    let detail = app.get(&format!("/api/groups/{group}"), &owner).await;
    assert_eq!(detail.body["my_position"], i32::MAX);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn invites_can_be_listed_and_revoked(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let (bob, _) = app.register("bob").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let invites = format!("/api/groups/{group}/invites");

    let used_up = app.post(&invites, &owner, json!({ "max_uses": 1 })).await;
    let code = used_up.body["code"].as_str().unwrap().to_owned();
    app.post(&format!("/api/invites/{code}/join"), &bob, json!({}))
        .await;
    let open = app
        .post(&invites, &owner, json!({ "expires_in_hours": 24 }))
        .await;
    let open_code = open.body["code"].as_str().unwrap().to_owned();

    // Used-up invites (and the one from invite_and_join) are no longer listed.
    let list = app.get(&invites, &owner).await;
    assert_eq!(list.status, StatusCode::OK);
    let codes: Vec<&str> = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&open_code.as_str()), "{codes:?}");
    assert!(!codes.contains(&code.as_str()), "{codes:?}");

    assert_eq!(
        app.get(&invites, &alice).await.status,
        StatusCode::FORBIDDEN
    );
    let res = app.delete(&format!("{invites}/{open_code}"), &alice).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app.delete(&format!("{invites}/{open_code}"), &owner).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = app
        .post(&format!("/api/invites/{open_code}/join"), &alice, json!({}))
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app.delete(&format!("{invites}/{open_code}"), &owner).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn new_roles_go_on_top_below_the_creator(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let roles = format!("/api/groups/{group}/roles");

    let a = app.post(&roles, &owner, json!({ "name": "A" })).await;
    assert_eq!(a.body["position"], 1);
    let b = app.post(&roles, &owner, json!({ "name": "B" })).await;
    assert_eq!(b.body["position"], 2);
    let admin = app
        .post(
            &roles,
            &owner,
            json!({ "name": "Admin", "permissions": "MANAGE_ROLES" }),
        )
        .await;
    assert_eq!(admin.body["position"], 3);
    let admin_id = admin.body["id"].as_str().unwrap();
    app.put(
        &format!("/api/groups/{group}/members/{alice_id}/roles/{admin_id}"),
        &owner,
        json!({}),
    )
    .await;

    // Alice ranks at 3, so her new role lands at 2, just below her.
    let c = app.post(&roles, &alice, json!({ "name": "C" })).await;
    assert_eq!(c.status, StatusCode::CREATED, "{:?}", c.body);
    assert_eq!(c.body["position"], 2);
}
