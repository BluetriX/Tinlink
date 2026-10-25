use std::sync::Arc;

use sqlx::PgPool;

use crate::config::Config;
use crate::events::EventHub;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub events: EventHub,
}
