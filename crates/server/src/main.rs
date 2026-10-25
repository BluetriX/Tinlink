use std::sync::Arc;

use anyhow::Result;
use tinlink_server::config::Config;
use tinlink_server::{AppState, connect_db, router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    if config.dev_insecure_tokens {
        tracing::warn!(
            "TINLINK_DEV_INSECURE_TOKENS is enabled: voice tokens are issued without authentication"
        );
    }
    let db = connect_db(&config.database_url).await?;

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(addr = %config.bind, domain = %config.domain, "tinlink server listening");

    let state = AppState {
        config,
        db,
        events: Default::default(),
    };
    tinlink_server::tasks::spawn(state.clone());

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
