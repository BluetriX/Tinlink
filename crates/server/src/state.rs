use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;
use crate::events::EventHub;
use crate::livekit::RoomService;
use crate::presence::Presence;
use crate::ratelimit::LoginLimiter;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub events: EventHub,
    pub login_limiter: Arc<LoginLimiter>,
    pub presence: Arc<Presence>,
    pub rooms: RoomService,
}

impl AppState {
    pub fn new(config: Config, db: PgPool) -> Self {
        Self {
            rooms: RoomService::new(config.livekit.clone()),
            config: Arc::new(config),
            db,
            events: EventHub::default(),
            login_limiter: Arc::default(),
            presence: Arc::default(),
        }
    }
}
