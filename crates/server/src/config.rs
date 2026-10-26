//! Server configuration, read from environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Public domain of this server, used in user ids (`name@domain`).
    pub domain: String,
    pub bind: SocketAddr,
    pub database_url: String,
    /// Allow anyone to register. The first account can always be created.
    pub open_registration: bool,
    pub livekit: LiveKitConfig,
    /// Issue voice tokens without authentication. Development only.
    pub dev_insecure_tokens: bool,
    /// Directory for uploaded files.
    pub files_dir: PathBuf,
    pub max_upload_bytes: usize,
    /// Where users can get the source code of this server (AGPL section 13).
    pub source_url: String,
    /// Static files of the web client, served at `/` when the directory exists.
    pub web_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LiveKitConfig {
    /// URL clients use to reach LiveKit, e.g. `wss://voice.example.org`.
    pub public_url: String,
    /// URL the server uses for LiveKit's API, e.g. `http://livekit:7880`.
    /// Without it, moderation of connected participants is unavailable.
    pub internal_url: Option<String>,
    pub api_key: String,
    pub api_secret: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            domain: required("TINLINK_DOMAIN")?,
            bind: optional("TINLINK_BIND")
                .unwrap_or_else(|| "0.0.0.0:8080".into())
                .parse()
                .context("TINLINK_BIND is not a valid socket address")?,
            database_url: required("DATABASE_URL")?,
            open_registration: optional("TINLINK_OPEN_REGISTRATION").is_none_or(|v| v == "true"),
            livekit: LiveKitConfig {
                public_url: required("LIVEKIT_PUBLIC_URL")?,
                internal_url: optional("LIVEKIT_INTERNAL_URL"),
                api_key: required("LIVEKIT_API_KEY")?,
                api_secret: required("LIVEKIT_API_SECRET")?,
            },
            dev_insecure_tokens: optional("TINLINK_DEV_INSECURE_TOKENS")
                .is_some_and(|v| v == "true"),
            files_dir: optional("TINLINK_FILES_DIR")
                .unwrap_or_else(|| "/data/files".into())
                .into(),
            max_upload_bytes: optional("TINLINK_MAX_UPLOAD_MB")
                .map(|v| v.parse::<usize>())
                .transpose()
                .context("TINLINK_MAX_UPLOAD_MB must be a number")?
                .unwrap_or(25)
                * 1024
                * 1024,
            source_url: optional("TINLINK_SOURCE_URL")
                .unwrap_or_else(|| env!("CARGO_PKG_REPOSITORY").into()),
            web_dir: Some(
                optional("TINLINK_WEB_DIR")
                    .unwrap_or_else(|| "/app/web".into())
                    .into(),
            ),
        })
    }
}

fn required(key: &str) -> Result<String> {
    optional(key).with_context(|| format!("environment variable {key} is required"))
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}
