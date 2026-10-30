//! Settings that must be known before the WebRTC runtime starts, kept in a
//! file next to the session. The UI keeps all other settings itself.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::StartupOptions;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EarlyPrefs {
    /// Show received video as soon as it is decoded instead of smoothing
    /// its playout. Takes effect when the app starts.
    pub low_latency_playback: bool,
}

impl Default for EarlyPrefs {
    fn default() -> Self {
        Self {
            low_latency_playback: true,
        }
    }
}

fn file(app: &AppHandle) -> Option<PathBuf> {
    let name = match &app.state::<StartupOptions>().profile {
        Some(profile) => format!("prefs-{profile}.json"),
        None => "prefs.json".into(),
    };
    Some(app.path().app_data_dir().ok()?.join(name))
}

/// The saved settings; defaults if there are none or they are unreadable.
pub fn load(app: &AppHandle) -> EarlyPrefs {
    file(app)
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, prefs: &EarlyPrefs) -> anyhow::Result<()> {
    let path = file(app).ok_or_else(|| anyhow::anyhow!("no app data directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(prefs)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_keep_the_defaults() {
        let prefs: EarlyPrefs = serde_json::from_str("{}").unwrap();
        assert!(prefs.low_latency_playback);
        let prefs: EarlyPrefs = serde_json::from_str(r#"{"lowLatencyPlayback":false}"#).unwrap();
        assert!(!prefs.low_latency_playback);
    }
}
