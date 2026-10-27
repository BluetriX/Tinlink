//! HTTP API routes.

mod attachments;
mod channels;
mod direct;
mod groups;
mod images;
mod messages;
mod roles;
mod voice;
mod webhook;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};

use crate::auth;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/users/me", get(auth::me).patch(auth::update_me))
        .route("/users/me/avatar", put(images::set_avatar))
        .route("/images/{id}", get(images::get))
        .route("/users/me/password", put(auth::change_password))
        .route("/info", get(info))
        .route("/dms", get(direct::list).post(direct::open))
        .route("/search", get(messages::search))
        .route("/groups", get(groups::list_mine).post(groups::create))
        .route(
            "/groups/{group_id}",
            get(groups::get)
                .patch(groups::update)
                .delete(groups::delete),
        )
        .route("/groups/{group_id}/icon", put(images::set_group_icon))
        .route("/groups/{group_id}/members", get(groups::list_members))
        .route(
            "/groups/{group_id}/members/{user_id}",
            delete(groups::remove_member),
        )
        .route(
            "/groups/{group_id}/members/{user_id}/roles/{role_id}",
            put(roles::assign).delete(roles::unassign),
        )
        .route(
            "/groups/{group_id}/invites",
            get(groups::list_invites).post(groups::create_invite),
        )
        .route(
            "/groups/{group_id}/invites/{code}",
            delete(groups::delete_invite),
        )
        .route("/invites/{code}/join", post(groups::join))
        .route(
            "/groups/{group_id}/roles",
            get(roles::list).post(roles::create),
        )
        .route(
            "/groups/{group_id}/roles/{role_id}",
            patch(roles::update).delete(roles::delete),
        )
        .route(
            "/groups/{group_id}/categories",
            post(channels::create_category),
        )
        .route(
            "/categories/{category_id}",
            patch(channels::update_category).delete(channels::delete_category),
        )
        .route(
            "/groups/{group_id}/channels",
            get(channels::list).post(channels::create),
        )
        .route(
            "/channels/{channel_id}",
            patch(channels::update).delete(channels::delete),
        )
        .route("/channels/{channel_id}/icon", put(images::set_channel_icon))
        .route(
            "/channels/{channel_id}/overwrites",
            get(channels::list_overwrites),
        )
        .route(
            "/channels/{channel_id}/overwrites/{kind}/{target_id}",
            put(channels::put_overwrite).delete(channels::delete_overwrite),
        )
        .route(
            "/channels/{channel_id}/messages",
            get(messages::list).post(messages::post),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}",
            patch(messages::edit).delete(messages::delete),
        )
        .route("/channels/{channel_id}/read", put(messages::read))
        .route(
            "/channels/{channel_id}/attachments",
            // Size is enforced while streaming to disk (TINLINK_MAX_UPLOAD_MB).
            post(attachments::upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/files/{id}", get(attachments::download))
        .route(
            "/channels/{channel_id}/voice-token",
            post(voice::channel_token),
        )
        .route(
            "/groups/{group_id}/members/{user_id}/voice-mute",
            put(voice::set_mute),
        )
        .route(
            "/channels/{channel_id}/voice/{user_id}/disconnect",
            post(voice::disconnect),
        )
        .route(
            "/channels/{channel_id}/voice/{user_id}/move",
            post(voice::move_member),
        )
        .route("/voice/token", post(voice::dev_token))
        .route("/livekit/webhook", post(webhook::receive))
}

/// Public server information, including where to get the source code
/// (required by the AGPL for network use).
async fn info(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "name": "Tinlink",
        "version": env!("CARGO_PKG_VERSION"),
        "domain": state.config.domain,
        "source_url": state.config.source_url,
        "open_registration": state.config.open_registration,
        "max_upload_bytes": state.config.max_upload_bytes,
    }))
}
