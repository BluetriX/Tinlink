//! End-to-end API tests against a real PostgreSQL database.
//!
//! Each test gets a fresh database (requires `DATABASE_URL` pointing at a
//! server where the user may create databases).

mod common;

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

fn jwt_payload(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap()
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn register_login_and_logout(db: PgPool) {
    let app = TestApp::new(db);
    let (token, _) = app.register("Erik").await;

    let me = app.get("/api/users/me", &token).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["name"], "erik");

    let dup = app
        .request(
            Method::POST,
            "/api/auth/register",
            None,
            Some(json!({"name": "erik", "password": "something long"})),
        )
        .await;
    assert_eq!(dup.status, StatusCode::CONFLICT);

    let short = app
        .request(
            Method::POST,
            "/api/auth/register",
            None,
            Some(json!({"name": "bob", "password": "short"})),
        )
        .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);

    let wrong = app
        .request(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"name": "erik", "password": "wrong password"})),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);

    let unknown = app
        .request(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"name": "nobody", "password": "whatever123"})),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    let login = app
        .request(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"name": "ERIK", "password": "correct horse battery"})),
        )
        .await;
    assert_eq!(login.status, StatusCode::OK);
    let token2 = login.body["token"].as_str().unwrap();

    let out = app
        .request(Method::POST, "/api/auth/logout", Some(token2), None)
        .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get("/api/users/me", token2).await.status,
        StatusCode::UNAUTHORIZED
    );
    // The other session stays valid.
    assert_eq!(
        app.get("/api/users/me", &token).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.get("/api/users/me", "garbage").await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn closed_registration_allows_only_first_user(db: PgPool) {
    let mut config = common::config();
    config.open_registration = false;
    let app = TestApp::with_config(db, config);
    app.register("admin").await;
    let res = app
        .request(
            Method::POST,
            "/api/auth/register",
            None,
            Some(json!({"name": "other", "password": "long enough pw"})),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn group_creation_and_visibility(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (stranger, _) = app.register("stranger").await;
    let group = app.create_group(&owner, "Friends").await;

    let mine = app.get("/api/groups", &owner).await;
    assert_eq!(mine.body.as_array().unwrap().len(), 1);
    assert_eq!(mine.body[0]["name"], "Friends");

    let channels = app.channels(&owner, &group).await;
    assert_eq!(channels["channels"][0]["name"], "general");
    assert_eq!(channels["categories"][0]["name"], "Rooms");
    assert_eq!(channels["categories"][0]["allow_user_channels"], true);

    // Non-members cannot even tell the group exists.
    assert_eq!(
        app.get(&format!("/api/groups/{group}"), &stranger)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.get(&format!("/api/groups/{group}/channels"), &stranger)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        app.get("/api/groups", &stranger)
            .await
            .body
            .as_array()
            .unwrap()
            .is_empty()
    );

    let bad = app
        .post("/api/groups", &owner, json!({"name": "   "}))
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn invites_respect_max_uses(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (a, _) = app.register("alice").await;
    let (b, _) = app.register("bob").await;
    let group = app.create_group(&owner, "G").await;

    let invite = app
        .post(
            &format!("/api/groups/{group}/invites"),
            &owner,
            json!({"max_uses": 1}),
        )
        .await;
    let code = invite.body["code"].as_str().unwrap();
    assert_eq!(
        app.post(&format!("/api/invites/{code}/join"), &a, json!({}))
            .await
            .status,
        StatusCode::OK
    );
    // Joining again does not consume a use and still succeeds.
    assert_eq!(
        app.post(&format!("/api/invites/{code}/join"), &a, json!({}))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.post(&format!("/api/invites/{code}/join"), &b, json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.post("/api/invites/doesnotexist/join", &b, json!({}))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let members = app.get(&format!("/api/groups/{group}/members"), &a).await;
    assert_eq!(members.body.as_array().unwrap().len(), 2);

    // Regular members cannot create invites.
    let res = app
        .post(&format!("/api/groups/{group}/invites"), &a, json!({}))
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn members_create_limited_own_channels(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let (bob, _) = app.register("bob").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    app.invite_and_join(&owner, &group, &bob).await;
    let rooms = app.category_id(&owner, &group, "Rooms").await;

    // Outside a category that allows it: rejected.
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &alice,
            json!({"name": "mine"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &alice,
            json!({"name": "Alice 1", "category_id": rooms, "temporary": true}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(res.body["temporary"], true);
    let own = res.body["id"].as_str().unwrap().to_owned();
    assert!(
        res.body["permissions"]
            .as_str()
            .unwrap()
            .contains("MANAGE_CHANNELS")
    );

    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &alice,
            json!({"name": "Alice 2", "category_id": rooms}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
    // Default limit is 2.
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &alice,
            json!({"name": "Alice 3", "category_id": rooms}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // The creator can rename the own channel, others cannot.
    let res = app
        .patch(
            &format!("/api/channels/{own}"),
            &alice,
            json!({"name": "Renamed"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "Renamed");
    let res = app
        .patch(
            &format!("/api/channels/{own}"),
            &bob,
            json!({"name": "Hijacked"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The owner raises the limit, then Alice can create another one.
    let res = app
        .patch(
            &format!("/api/groups/{group}"),
            &owner,
            json!({"user_channel_limit": 3}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &alice,
            json!({"name": "Alice 3", "category_id": rooms}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Owners create regular channels anywhere; temporary is ignored for them.
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({"name": "Announcements", "voice_enabled": false, "temporary": true}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["created_by"], Value::Null);
    assert_eq!(res.body["temporary"], false);

    // Bob deleting Alice's channel is forbidden, Alice may delete it.
    assert_eq!(
        app.delete(&format!("/api/channels/{own}"), &bob)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.delete(&format!("/api/channels/{own}"), &alice)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn private_channels_via_overwrites_and_roles(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;

    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({"name": "staff"}),
        )
        .await;
    let staff = res.body["id"].as_str().unwrap().to_owned();

    // Hide the channel from @everyone (role id == group id).
    let res = app
        .put(
            &format!("/api/channels/{staff}/overwrites/role/{group}"),
            &owner,
            json!({"deny": "VIEW_CHANNEL"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{:?}", res.body);
    assert!(app.channel_id(&alice, &group, "staff").await.is_none());
    assert_eq!(
        app.get(&format!("/api/channels/{staff}/messages"), &alice)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // A role that is allowed to see it.
    let res = app
        .post(
            &format!("/api/groups/{group}/roles"),
            &owner,
            json!({"name": "Staff"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    let role = res.body["id"].as_str().unwrap().to_owned();
    let res = app
        .put(
            &format!("/api/channels/{staff}/overwrites/role/{role}"),
            &owner,
            json!({"allow": "VIEW_CHANNEL"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = app
        .put(
            &format!("/api/groups/{group}/members/{alice_id}/roles/{role}"),
            &owner,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{:?}", res.body);
    assert!(app.channel_id(&alice, &group, "staff").await.is_some());

    // A member overwrite beats the role overwrite.
    let res = app
        .put(
            &format!("/api/channels/{staff}/overwrites/member/{alice_id}"),
            &owner,
            json!({"deny": "VIEW_CHANNEL"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(app.channel_id(&alice, &group, "staff").await.is_none());

    let res = app
        .delete(
            &format!("/api/channels/{staff}/overwrites/member/{alice_id}"),
            &owner,
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(app.channel_id(&alice, &group, "staff").await.is_some());

    // Unknown targets and kinds are rejected.
    let res = app
        .put(
            &format!("/api/channels/{staff}/overwrites/role/{alice_id}"),
            &owner,
            json!({"allow": "VIEW_CHANNEL"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = app
        .put(
            &format!("/api/channels/{staff}/overwrites/robot/{alice_id}"),
            &owner,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn role_hierarchy_is_enforced(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (moder, mod_id) = app.register("moderator").await;
    let (alice, alice_id) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &moder).await;
    app.invite_and_join(&owner, &group, &alice).await;

    let res = app
        .post(
            &format!("/api/groups/{group}/roles"),
            &owner,
            json!({"name": "Mod", "position": 10, "permissions": "MANAGE_ROLES | KICK_MEMBERS | MANAGE_MESSAGES"}),
        )
        .await;
    let mod_role = res.body["id"].as_str().unwrap().to_owned();
    app.put(
        &format!("/api/groups/{group}/members/{mod_id}/roles/{mod_role}"),
        &owner,
        json!({}),
    )
    .await;

    // Cannot create a role at or above the own position.
    let res = app
        .post(
            &format!("/api/groups/{group}/roles"),
            &moder,
            json!({"name": "Super", "position": 10}),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // Cannot grant permissions the moderator does not have.
    let res = app
        .post(
            &format!("/api/groups/{group}/roles"),
            &moder,
            json!({"name": "Admin", "position": 5, "permissions": "ADMINISTRATOR"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // Allowed: a lower role with a subset of the own permissions.
    let res = app
        .post(
            &format!("/api/groups/{group}/roles"),
            &moder,
            json!({"name": "Helper", "position": 5, "permissions": "MANAGE_MESSAGES"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let helper = res.body["id"].as_str().unwrap().to_owned();
    let res = app
        .put(
            &format!("/api/groups/{group}/members/{alice_id}/roles/{helper}"),
            &moder,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    // The moderator cannot remove or edit their own (equal) role.
    let res = app
        .patch(
            &format!("/api/groups/{group}/roles/{mod_role}"),
            &moder,
            json!({"name": "x"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app
        .delete(&format!("/api/groups/{group}/roles/{mod_role}"), &moder)
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Kicking: moderator can kick alice (lower), alice cannot kick anyone.
    let res = app
        .delete(&format!("/api/groups/{group}/members/{mod_id}"), &alice)
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = app
        .delete(&format!("/api/groups/{group}/members/{alice_id}"), &moder)
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get(&format!("/api/groups/{group}"), &alice)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Roles list shows @everyone flagged.
    let roles = app.get(&format!("/api/groups/{group}/roles"), &owner).await;
    let everyone = roles
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["everyone"] == true)
        .unwrap();
    assert_eq!(everyone["id"].as_str().unwrap(), group);
    let res = app
        .delete(&format!("/api/groups/{group}/roles/{group}"), &owner)
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // The owner cannot leave; the moderator can.
    let owner_id = app.get("/api/users/me", &owner).await.body["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        app.delete(&format!("/api/groups/{group}/members/{owner_id}"), &owner)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.delete(&format!("/api/groups/{group}/members/{mod_id}"), &moder)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn chat_messages(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();
    let path = format!("/api/channels/{general}/messages");

    let mut ids = Vec::new();
    for i in 0..5 {
        let res = app
            .post(&path, &alice, json!({"content": format!("msg {i}")}))
            .await;
        assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
        assert_eq!(res.body["author_name"], "alice");
        ids.push(res.body["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        app.post(&path, &alice, json!({"content": "  "}))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    let page = app.get(&format!("{path}?limit=2"), &owner).await;
    let contents: Vec<&str> = page
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["msg 4", "msg 3"]);
    let page = app
        .get(&format!("{path}?limit=10&before={}", ids[2]), &owner)
        .await;
    let contents: Vec<&str> = page
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["msg 1", "msg 0"]);

    // Only the author edits.
    let res = app
        .patch(
            &format!("{path}/{}", ids[0]),
            &owner,
            json!({"content": "nope"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = app
        .patch(
            &format!("{path}/{}", ids[0]),
            &alice,
            json!({"content": "edited"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["edited_at"].is_string());

    // Owner (all permissions) may delete others' messages; alice her own.
    assert_eq!(
        app.delete(&format!("{path}/{}", ids[1]), &owner)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.delete(&format!("{path}/{}", ids[2]), &alice)
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // A message by the owner cannot be deleted by alice.
    let res = app
        .post(&path, &owner, json!({"content": "owner says"}))
        .await;
    let owner_msg = res.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        app.delete(&format!("{path}/{owner_msg}"), &alice)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Read-only channel for @everyone.
    let res = app
        .put(
            &format!("/api/channels/{general}/overwrites/role/{group}"),
            &owner,
            json!({"deny": "SEND_MESSAGES"}),
        )
        .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.post(&path, &alice, json!({"content": "hi"}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(app.get(&path, &alice).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn voice_tokens_reflect_channel_permissions(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let res = app
        .post(
            &format!("/api/channels/{general}/voice-token"),
            &alice,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(res.body["identity"], "alice@test.local");
    assert_eq!(res.body["room"], general.as_str());
    assert_eq!(res.body["url"], "ws://livekit.test");
    let claims = jwt_payload(res.body["token"].as_str().unwrap());
    assert_eq!(claims["iss"], "testkey");
    assert_eq!(claims["video"]["room"], general.as_str());
    assert_eq!(
        claims["video"]["canPublishSources"],
        json!(["microphone", "screen_share", "screen_share_audio"])
    );

    // No screen sharing for @everyone in this channel.
    app.put(
        &format!("/api/channels/{general}/overwrites/role/{group}"),
        &owner,
        json!({"deny": "SHARE_SCREEN"}),
    )
    .await;
    let res = app
        .post(
            &format!("/api/channels/{general}/voice-token"),
            &alice,
            json!({}),
        )
        .await;
    let claims = jwt_payload(res.body["token"].as_str().unwrap());
    assert_eq!(claims["video"]["canPublishSources"], json!(["microphone"]));

    // No connect at all.
    app.put(
        &format!("/api/channels/{general}/overwrites/role/{group}"),
        &owner,
        json!({"deny": "CONNECT"}),
    )
    .await;
    let res = app
        .post(
            &format!("/api/channels/{general}/voice-token"),
            &alice,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Text-only channel.
    let res = app
        .post(
            &format!("/api/groups/{group}/channels"),
            &owner,
            json!({"name": "text", "voice_enabled": false}),
        )
        .await;
    let text = res.body["id"].as_str().unwrap().to_owned();
    let res = app
        .post(
            &format!("/api/channels/{text}/voice-token"),
            &owner,
            json!({}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // The development endpoint is disabled by default.
    let res = app
        .request(
            Method::POST,
            "/api/voice/token",
            None,
            Some(json!({"name": "x", "room": "lobby"})),
        )
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn dev_token_endpoint_when_enabled(db: PgPool) {
    let mut config = common::config();
    config.dev_insecure_tokens = true;
    let app = TestApp::with_config(db, config);
    let res = app
        .request(
            Method::POST,
            "/api/voice/token",
            None,
            Some(json!({"name": "Erik", "room": "lobby"})),
        )
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["identity"], "erik@test.local");
    let res = app
        .request(
            Method::POST,
            "/api/voice/token",
            None,
            Some(json!({"name": "bad name", "room": "lobby"})),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn only_owner_deletes_group(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    assert_eq!(
        app.delete(&format!("/api/groups/{group}"), &alice)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.delete(&format!("/api/groups/{group}"), &owner)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(
        app.get("/api/groups", &alice)
            .await
            .body
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn login_is_rate_limited_per_user(db: PgPool) {
    let app = TestApp::new(db);
    app.register("erik").await;
    let login = |password: &'static str| {
        let app = &app;
        async move {
            app.request(
                Method::POST,
                "/api/auth/login",
                None,
                Some(json!({"name": "erik", "password": password})),
            )
            .await
            .status
        }
    };
    for _ in 0..10 {
        assert_eq!(login("wrong password").await, StatusCode::UNAUTHORIZED);
    }
    // Even the right password is refused while blocked.
    assert_eq!(
        login("correct horse battery").await,
        StatusCode::TOO_MANY_REQUESTS
    );
}
