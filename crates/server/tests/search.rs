//! Message search.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

fn contents(res: &Value) -> Vec<String> {
    res.as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn search_finds_only_visible_messages(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let (bob, _) = app.register("bob").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    app.invite_and_join(&owner, &group, &bob).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    // A channel alice cannot see.
    let secret = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({"name": "secret"}),
        )
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let res = app
        .put(
            &format!("/api/channels/{secret}/overwrites/role/{group}"),
            &owner,
            json!({"deny": "VIEW_CHANNEL"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{:?}", res.body);

    let post = |channel: String, token: String, content: &'static str| {
        let app = &app;
        async move {
            let res = app
                .post(
                    &format!("/api/channels/{channel}/messages"),
                    &token,
                    json!({ "content": content }),
                )
                .await;
            assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
        }
    };
    post(general.clone(), owner.clone(), "Pizza am Freitag?").await;
    post(general.clone(), alice.clone(), "Lieber Burger, keine pizza").await;
    post(general.clone(), alice.clone(), "Grüße aus Köln").await;
    post(secret.clone(), owner.clone(), "geheime Pizzaparty").await;
    let dm = app
        .post("/api/dms", &alice, json!({ "user_id": owner_id }))
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_owned();
    post(dm.clone(), owner.clone(), "pizza nur für dich").await;

    // Prefix match, case-insensitive, newest first; the hidden channel is excluded.
    let res = app.get("/api/search?q=PIZ", &alice).await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(
        contents(&res.body),
        [
            "pizza nur für dich",
            "Lieber Burger, keine pizza",
            "Pizza am Freitag?"
        ]
    );
    assert_eq!(res.body[0]["group_id"], Value::Null);
    assert_eq!(res.body[0]["channel_name"], "owner");
    assert_eq!(res.body[1]["group_id"], group.as_str());
    assert_eq!(res.body[1]["channel_name"], "general");

    // All words must match; umlauts work.
    let res = app.get("/api/search?q=pizza%20freit", &alice).await;
    assert_eq!(contents(&res.body), ["Pizza am Freitag?"]);
    let res = app.get("/api/search?q=gr%C3%BC%C3%9Fe", &alice).await;
    assert_eq!(contents(&res.body), ["Grüße aus Köln"]);

    // The owner sees the secret channel; bob does not see the DM.
    let res = app.get("/api/search?q=pizza", &owner).await;
    assert_eq!(contents(&res.body).len(), 4);
    let res = app.get("/api/search?q=pizza", &bob).await;
    assert_eq!(contents(&res.body).len(), 2);

    // Scopes and paging.
    let res = app
        .get(&format!("/api/search?q=pizza&group_id={group}"), &alice)
        .await;
    assert_eq!(contents(&res.body).len(), 2);
    let res = app
        .get(&format!("/api/search?q=pizza&channel_id={dm}"), &alice)
        .await;
    assert_eq!(contents(&res.body), ["pizza nur für dich"]);
    let res = app.get("/api/search?q=pizza&limit=1", &alice).await;
    let first = res.body[0]["id"].as_str().unwrap().to_owned();
    let res = app
        .get(
            &format!("/api/search?q=pizza&limit=1&before={first}"),
            &alice,
        )
        .await;
    assert_eq!(contents(&res.body), ["Lieber Burger, keine pizza"]);

    // No access to other channels or groups, and empty queries are rejected.
    let res = app
        .get(&format!("/api/search?q=pizza&channel_id={secret}"), &alice)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app
        .get(&format!("/api/search?q=pizza&channel_id={dm}"), &bob)
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app.get("/api/search?q=%3F%21", &alice).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}
