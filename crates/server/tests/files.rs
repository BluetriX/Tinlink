//! File uploads, signed downloads and cleanup.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::TestApp;
use serde_json::json;
use sqlx::PgPool;

async fn download(app: &TestApp, url: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    app.raw(Request::builder().uri(url).body(Body::empty()).unwrap())
        .await
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn upload_attach_and_download(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let png = b"\x89PNG\r\n\x1a\nfake image";
    let up = app
        .upload(&general, &alice, "../holiday.png", "image/png", png)
        .await;
    assert_eq!(up.status, StatusCode::CREATED, "{:?}", up.body);
    assert_eq!(up.body["filename"], "holiday.png");
    assert_eq!(up.body["size_bytes"], png.len());
    let id = up.body["id"].as_str().unwrap().to_owned();
    let url = up.body["url"].as_str().unwrap().to_owned();

    // Not downloadable before it is part of a message.
    assert_eq!(download(&app, &url).await.0, StatusCode::NOT_FOUND);

    // Others cannot use alice's upload.
    let res = app
        .post(
            &format!("/api/channels/{general}/messages"),
            &owner,
            json!({"attachment_ids": [id]}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = app
        .post(
            &format!("/api/channels/{general}/messages"),
            &alice,
            json!({"attachment_ids": [id]}),
        )
        .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(res.body["content"], "");
    assert_eq!(res.body["attachments"][0]["id"], id.as_str());
    // An attachment can only be linked once.
    let res = app
        .post(
            &format!("/api/channels/{general}/messages"),
            &alice,
            json!({"content": "again", "attachment_ids": [id]}),
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let history = app
        .get(&format!("/api/channels/{general}/messages"), &owner)
        .await;
    let url = history.body[0]["attachments"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, headers, body) = download(&app, &url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, png);
    assert_eq!(headers["content-type"], "image/png");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("inline")
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");

    // Tampered signatures or ids are rejected.
    let tampered = url.replace("sig=", "sig=AAAA");
    assert_eq!(download(&app, &tampered).await.0, StatusCode::NOT_FOUND);
    let other_id = url.replacen(&id, "00000000-0000-0000-0000-000000000000", 1);
    assert_eq!(download(&app, &other_id).await.0, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn risky_types_are_served_as_downloads(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let group = app.create_group(&owner, "G").await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let up = app
        .upload(
            &general,
            &owner,
            "evil.html",
            "text/html",
            b"<script>alert(1)</script>",
        )
        .await;
    let id = up.body["id"].as_str().unwrap().to_owned();
    let res = app
        .post(
            &format!("/api/channels/{general}/messages"),
            &owner,
            json!({"content": "see", "attachment_ids": [id]}),
        )
        .await;
    let url = res.body["attachments"][0]["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, headers, _) = download(&app, &url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn upload_limits_and_permissions(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let (alice, _) = app.register("alice").await;
    let group = app.create_group(&owner, "G").await;
    app.invite_and_join(&owner, &group, &alice).await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    // The test config allows 1 MB.
    let big = vec![0u8; 1024 * 1024 + 1];
    let res = app
        .upload(
            &general,
            &alice,
            "big.bin",
            "application/octet-stream",
            &big,
        )
        .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let dir = &app.state.config.files_dir;
    let leftover = std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftover, 0, "partial upload was not removed");

    app.put(
        &format!("/api/channels/{general}/overwrites/role/{group}"),
        &owner,
        json!({"deny": "ATTACH_FILES"}),
    )
    .await;
    let res = app
        .upload(&general, &alice, "a.txt", "text/plain", b"hi")
        .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn orphaned_uploads_are_swept(db: PgPool) {
    let app = TestApp::new(db);
    let (owner, _) = app.register("owner").await;
    let group = app.create_group(&owner, "G").await;
    let general = app.channel_id(&owner, &group, "general").await.unwrap();

    let unused = app
        .upload(&general, &owner, "unused.txt", "text/plain", b"x")
        .await;
    let unused_id = unused.body["id"].as_str().unwrap().to_owned();
    let kept = app
        .upload(&general, &owner, "kept.txt", "text/plain", b"y")
        .await;
    let kept_id = kept.body["id"].as_str().unwrap().to_owned();
    app.post(
        &format!("/api/channels/{general}/messages"),
        &owner,
        json!({"content": "k", "attachment_ids": [kept_id]}),
    )
    .await;
    let dir = app.state.config.files_dir.clone();

    tinlink_server::tasks::sweep(&app.state, Duration::from_secs(3600), Duration::ZERO)
        .await
        .unwrap();
    assert!(!dir.join(&unused_id).exists());
    assert!(dir.join(&kept_id).exists());

    // Deleting the channel removes the rows; the file goes on the next sweep.
    app.delete(&format!("/api/channels/{general}"), &owner)
        .await;
    tinlink_server::tasks::sweep(&app.state, Duration::from_secs(3600), Duration::ZERO)
        .await
        .unwrap();
    assert!(!dir.join(&kept_id).exists());
}

#[sqlx::test(migrator = "tinlink_server::MIGRATOR")]
async fn info_exposes_source_url(db: PgPool) {
    let app = TestApp::new(db);
    let res = app
        .request(axum::http::Method::GET, "/api/info", None, None)
        .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["source_url"], "https://example.org/tinlink");
    assert_eq!(res.body["name"], "Tinlink");
}
