//! Test helpers: an in-process app backed by a fresh database per test.
#![allow(dead_code)] // each test binary uses a different subset of the helpers

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tinlink_server::config::{Config, LiveKitConfig};
use tinlink_server::{AppState, router};
use tower::ServiceExt;

pub const LIVEKIT_SECRET: &str = "test-secret-that-is-at-least-32-bytes";

pub struct TestApp {
    router: Router,
    pub state: AppState,
}

impl TestApp {
    /// Serves the app on a random local port for WebSocket tests and
    /// returns its address. HTTP requests through `request` share its state.
    pub async fn spawn(&self) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = self.router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }
}

pub struct Response {
    pub status: StatusCode,
    pub body: Value,
}

pub fn config() -> Config {
    Config {
        domain: "test.local".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url: String::new(),
        open_registration: true,
        livekit: LiveKitConfig {
            public_url: "ws://livekit.test".into(),
            api_key: "testkey".into(),
            api_secret: LIVEKIT_SECRET.into(),
        },
        dev_insecure_tokens: false,
    }
}

impl TestApp {
    pub fn new(db: PgPool) -> Self {
        Self::with_config(db, config())
    }

    pub fn with_config(db: PgPool, config: Config) -> Self {
        let state = AppState {
            config: Arc::new(config),
            db,
            events: Default::default(),
        };
        Self {
            router: router(state.clone()),
            state,
        }
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Response {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        Response { status, body }
    }

    pub async fn get(&self, path: &str, token: &str) -> Response {
        self.request(Method::GET, path, Some(token), None).await
    }

    pub async fn post(&self, path: &str, token: &str, body: Value) -> Response {
        self.request(Method::POST, path, Some(token), Some(body))
            .await
    }

    pub async fn put(&self, path: &str, token: &str, body: Value) -> Response {
        self.request(Method::PUT, path, Some(token), Some(body))
            .await
    }

    pub async fn patch(&self, path: &str, token: &str, body: Value) -> Response {
        self.request(Method::PATCH, path, Some(token), Some(body))
            .await
    }

    pub async fn delete(&self, path: &str, token: &str) -> Response {
        self.request(Method::DELETE, path, Some(token), None).await
    }

    /// Registers a user and returns (token, user id).
    pub async fn register(&self, name: &str) -> (String, String) {
        let res = self
            .request(
                Method::POST,
                "/api/auth/register",
                None,
                Some(json!({ "name": name, "password": "correct horse battery" })),
            )
            .await;
        assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
        (
            res.body["token"].as_str().unwrap().to_owned(),
            res.body["user"]["id"].as_str().unwrap().to_owned(),
        )
    }

    /// Creates a group owned by `token` and returns its id.
    pub async fn create_group(&self, token: &str, name: &str) -> String {
        let res = self
            .post("/api/groups", token, json!({ "name": name }))
            .await;
        assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
        res.body["id"].as_str().unwrap().to_owned()
    }

    /// Creates an invite and lets `joiner` join the group.
    pub async fn invite_and_join(&self, owner: &str, group: &str, joiner: &str) {
        let invite = self
            .post(&format!("/api/groups/{group}/invites"), owner, json!({}))
            .await;
        assert_eq!(invite.status, StatusCode::CREATED, "{:?}", invite.body);
        let code = invite.body["code"].as_str().unwrap();
        let res = self
            .post(&format!("/api/invites/{code}/join"), joiner, json!({}))
            .await;
        assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    }

    pub async fn channels(&self, token: &str, group: &str) -> Value {
        let res = self
            .get(&format!("/api/groups/{group}/channels"), token)
            .await;
        assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
        res.body
    }

    /// Returns the id of the channel named `name` as seen by `token`.
    pub async fn channel_id(&self, token: &str, group: &str, name: &str) -> Option<String> {
        self.channels(token, group).await["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["id"].as_str().unwrap().to_owned())
    }

    pub async fn category_id(&self, token: &str, group: &str, name: &str) -> String {
        self.channels(token, group).await["categories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["id"].as_str().unwrap().to_owned())
            .unwrap()
    }
}
