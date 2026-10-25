//! HTTP API routes.

mod channels;
mod groups;
mod messages;
mod roles;
mod voice;
mod webhook;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};

use crate::auth;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/users/me", get(auth::me))
        .route("/groups", get(groups::list_mine).post(groups::create))
        .route(
            "/groups/{group_id}",
            get(groups::get)
                .patch(groups::update)
                .delete(groups::delete),
        )
        .route("/groups/{group_id}/members", get(groups::list_members))
        .route(
            "/groups/{group_id}/members/{user_id}",
            delete(groups::remove_member),
        )
        .route(
            "/groups/{group_id}/members/{user_id}/roles/{role_id}",
            put(roles::assign).delete(roles::unassign),
        )
        .route("/groups/{group_id}/invites", post(groups::create_invite))
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
            "/groups/{group_id}/channels",
            get(channels::list).post(channels::create),
        )
        .route(
            "/channels/{channel_id}",
            patch(channels::update).delete(channels::delete),
        )
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
        .route(
            "/channels/{channel_id}/voice-token",
            post(voice::channel_token),
        )
        .route("/voice/token", post(voice::dev_token))
        .route("/livekit/webhook", post(webhook::receive))
}
