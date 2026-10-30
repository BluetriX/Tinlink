//! Identity keys, devices and wrapped channel keys for end-to-end encryption.

mod common;

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// The server cannot check keys, so any bytes of the right length do.
fn bytes(fill: u8, len: usize) -> String {
    STANDARD.encode(vec![fill; len])
}

async fn add_device(app: &TestApp, token: &str, certified: bool) -> String {
    let id = Uuid::new_v4().to_string();
    let mut body = json!({
        "id": id, "kind": "device", "name": "PC",
        "sign_key": bytes(1, 32), "dh_key": bytes(2, 32),
    });
    if certified {
        body["certificate"] = bytes(3, 64).into();
    }
    let res = app.post("/api/devices", token, body).await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    id
}

fn wrapped(device: &str) -> Value {
    json!({ "device_id": device, "wrapped": bytes(4, 80), "signature": bytes(5, 64) })
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn identity_key_is_set_once(db: PgPool) {
    let app = TestApp::new(db);
    let (alice, _) = app.register("alice").await;
    let device = add_device(&app, &alice, true).await;

    let set = |key: u8, reset: bool| json!({ "identity_key": bytes(key, 32), "identity_backup": bytes(9, 100), "reset": reset });
    let res = app
        .put("/api/users/me/identity", &alice, set(7, false))
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{:?}", res.body);
    let res = app
        .put(
            &format!("/api/devices/{device}/certificate"),
            &alice,
            json!({ "certificate": bytes(6, 64) }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    // The same key again only updates the backup.
    let res = app
        .put("/api/users/me/identity", &alice, set(7, false))
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = app
        .put("/api/users/me/identity", &alice, set(8, false))
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = app.get("/api/users/me/identity-backup", &alice).await;
    assert_eq!(res.body["identity_key"], bytes(7, 32));
    assert_eq!(res.body["identity_backup"], bytes(9, 100));

    // A reset revokes every device signed by the old key.
    let res = app
        .put("/api/users/me/identity", &alice, set(8, true))
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let me = app.get("/api/users/me", &alice).await.body["id"].clone();
    let keys = app
        .get(&format!("/api/users/{}/keys", me.as_str().unwrap()), &alice)
        .await;
    assert_eq!(keys.body["identity_key"], bytes(8, 32));
    assert_eq!(keys.body["devices"], json!([]));
    // What the revoked device signed stays checkable with the old key.
    let former = &keys.body["former_devices"];
    assert_eq!(former.as_array().unwrap().len(), 1, "{former:?}");
    assert_eq!(former[0]["id"], device.as_str());
    assert_eq!(former[0]["identity_key"], bytes(7, 32));
    assert_eq!(former[0]["certificate"], bytes(6, 64));
    let res = app.delete(&format!("/api/devices/{device}"), &alice).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn devices_and_their_visibility(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let (stranger, _) = app.register("stranger").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;

    let approved = add_device(&app, &alice, true).await;
    let pending = add_device(&app, &alice, false).await;
    let keys = format!("/api/users/{alice_id}/keys");

    // Others only see approved devices; alice also sees the pending one.
    let res = app.get(&keys, &owner).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["devices"].as_array().unwrap().len(), 1);
    assert_eq!(res.body["devices"][0]["id"], approved.as_str());
    assert_eq!(res.body["devices"][0]["dh_key"], bytes(2, 32));
    let res = app.get(&keys, &alice).await;
    assert_eq!(res.body["devices"].as_array().unwrap().len(), 2);
    let res = app.get(&keys, &stranger).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    let res = app
        .put(
            &format!("/api/devices/{pending}/certificate"),
            &owner,
            json!({ "certificate": bytes(3, 64) }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "only the owner approves");
    let res = app
        .put(
            &format!("/api/devices/{pending}/certificate"),
            &alice,
            json!({ "certificate": bytes(3, 64) }),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get(&keys, &owner).await.body["devices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let res = app
        .delete(&format!("/api/devices/{approved}"), &alice)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get(&keys, &owner).await.body["devices"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Bad sizes, a second backup.
    let res = app
        .post(
            "/api/devices",
            &alice,
            json!({ "id": Uuid::new_v4(), "kind": "device", "name": "x",
                    "sign_key": bytes(1, 31), "dh_key": bytes(2, 32) }),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let backup = |id: Uuid| {
        json!({ "id": id, "kind": "backup", "name": "Recovery key",
                "dh_key": bytes(2, 32), "certificate": bytes(3, 64) })
    };
    let res = app
        .post("/api/devices", &alice, backup(Uuid::new_v4()))
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = app
        .post("/api/devices", &alice, backup(Uuid::new_v4()))
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn channel_keys_follow_access(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let (stranger, _) = app.register("stranger").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    let keys = format!("/api/channels/{general}/keys");

    let owner_pc = add_device(&app, &owner, true).await;
    let alice_pc = add_device(&app, &alice, true).await;
    let alice_new = add_device(&app, &alice, false).await;
    let stranger_pc = add_device(&app, &stranger, true).await;

    let res = app.get(&format!("{keys}?device={owner_pc}"), &owner).await;
    assert_eq!(
        res.body,
        json!({ "epoch": null, "rotate": false, "keys": [] })
    );

    // Keys only go to approved devices of readers, the first epoch is 1.
    let post = |epoch: i32, from: &str, to: &[&str]| {
        json!({ "device_id": from, "epoch": epoch,
                "keys": to.iter().map(|d| wrapped(d)).collect::<Vec<_>>() })
    };
    for to in [&alice_new, &stranger_pc] {
        let res = app
            .post(&keys, &owner, post(1, &owner_pc, &[&owner_pc, to]))
            .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{to}");
    }
    let res = app
        .post(&keys, &owner, post(2, &owner_pc, &[&owner_pc]))
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = app
        .post(&keys, &owner, post(1, &owner_pc, &[&alice_pc]))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "own device missing");
    let res = app
        .post(&keys, &alice, post(1, &owner_pc, &[&owner_pc]))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "someone else's device");
    let res = app
        .post(&keys, &stranger, post(1, &stranger_pc, &[&stranger_pc]))
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    let res = app
        .post(&keys, &owner, post(1, &owner_pc, &[&owner_pc, &alice_pc]))
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(res.body["added"], 2);

    let res = app.get(&format!("{keys}?device={alice_pc}"), &alice).await;
    assert_eq!(res.body["epoch"], 1);
    assert_eq!(res.body["rotate"], false);
    assert_eq!(res.body["keys"][0]["sender_id"], owner_pc.as_str());
    assert!(res.body["keys"][0]["sender_user_id"].is_string());
    assert_eq!(res.body["keys"][0]["wrapped"], bytes(4, 80));
    let res = app.get(&format!("{keys}/devices"), &alice).await;
    let users = res.body.as_array().unwrap();
    assert_eq!(users.len(), 2);
    let alice_keys = users
        .iter()
        .find(|u| u["user_id"] == alice_id.as_str())
        .unwrap();
    assert_eq!(
        alice_keys["devices"].as_array().unwrap().len(),
        1,
        "pending device left out"
    );
    let res = app.get(&format!("{keys}/devices"), &stranger).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app.get(&format!("{keys}?device={owner_pc}"), &alice).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "not alice's device");

    // A newly approved device shows up as missing the key, and alice can
    // share it, but only epochs she holds.
    app.put(
        &format!("/api/devices/{alice_new}/certificate"),
        &alice,
        json!({ "certificate": bytes(3, 64) }),
    )
    .await;
    let res = app
        .get(&format!("{keys}/missing?device={alice_pc}"), &alice)
        .await;
    assert_eq!(
        res.body,
        json!([{ "device_id": alice_new, "user_id": alice_id, "epochs": [1] }])
    );
    let res = app
        .post(&keys, &alice, post(1, &alice_new, &[&alice_pc]))
        .await;
    // Another device of the same user holds epoch 1, which is enough.
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["added"], 0);
    let res = app
        .post(&keys, &alice, post(1, &alice_pc, &[&alice_new]))
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = app
        .get(&format!("{keys}/missing?device={alice_pc}"), &alice)
        .await;
    assert_eq!(res.body, json!([]));

    // Alice leaves: the key she holds has to be replaced.
    let res = app
        .delete(&format!("/api/groups/{group}/members/{alice_id}"), &owner)
        .await;
    assert!(res.status.is_success(), "{:?}", res.body);
    let res = app.get(&format!("{keys}?device={owner_pc}"), &owner).await;
    assert_eq!(res.body["rotate"], true);
    let res = app
        .post(&keys, &owner, post(2, &owner_pc, &[&owner_pc, &alice_pc]))
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "alice is no reader");
    let res = app
        .post(&keys, &owner, post(2, &owner_pc, &[&owner_pc]))
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = app.get(&format!("{keys}?device={owner_pc}"), &owner).await;
    assert_eq!(res.body["epoch"], 2);
    assert_eq!(res.body["rotate"], false);

    // A revoked device also forces a new epoch.
    let res = app
        .delete(&format!("/api/devices/{owner_pc}"), &owner)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let second = add_device(&app, &owner, true).await;
    let res = app.get(&format!("{keys}?device={second}"), &owner).await;
    assert_eq!(res.body["rotate"], true);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn direct_message_keys(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let dm = app
        .post("/api/dms", &alice, json!({ "user_id": owner_id }))
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let owner_pc = add_device(&app, &owner, true).await;
    let alice_pc = add_device(&app, &alice, true).await;

    let res = app
        .post(
            &format!("/api/channels/{dm}/keys"),
            &alice,
            json!({ "device_id": alice_pc, "epoch": 1,
                    "keys": [wrapped(&alice_pc), wrapped(&owner_pc)] }),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    let res = app
        .get(
            &format!("/api/channels/{dm}/keys?device={owner_pc}"),
            &owner,
        )
        .await;
    assert_eq!(res.body["keys"].as_array().unwrap().len(), 1);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn encrypted_messages(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, owner_id) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let (_, stranger_id) = app.register("stranger").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    let msgs = format!("/api/channels/{general}/messages");

    let envelope = bytes(7, 200);
    let res = app
        .post(
            &msgs,
            &owner,
            json!({ "content": envelope, "encrypted": true,
                    "mentions": [alice_id, stranger_id, owner_id] }),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(res.body["encrypted"], true);
    assert_eq!(res.body["content"], envelope.as_str());
    // Only members other than the author count as mentioned.
    assert_eq!(res.body["mentions"], json!([alice_id]));
    let id = res.body["id"].as_str().unwrap().to_owned();

    let res = app
        .post(
            &msgs,
            &owner,
            json!({ "content": "not base64!", "encrypted": true }),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Search cannot look into it.
    let res = app
        .post(&msgs, &owner, json!({ "content": "plain words" }))
        .await;
    assert_eq!(res.body["encrypted"], false);
    let hits = app.get("/api/search?q=B", &alice).await;
    assert_eq!(hits.body, json!([]));
    let hits = app.get("/api/search?q=plain", &alice).await;
    assert_eq!(hits.body.as_array().unwrap().len(), 1);

    let res = app
        .patch(
            &format!("{msgs}/{id}"),
            &owner,
            json!({ "content": bytes(8, 100), "encrypted": true, "mentions": [] }),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(res.body["mentions"], json!([]));
    let list = app.get(&msgs, &alice).await;
    assert_eq!(list.body[1]["content"], bytes(8, 100));
    assert_eq!(list.body[1]["encrypted"], true);
}
