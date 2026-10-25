//! Push-to-talk key, read by polling instead of a registered hotkey so the
//! key still reaches games and other programs, and mouse buttons work.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};
use tinlink_winput::Hotkey;

use crate::AppState;

/// Polling interval: at most this much delay between pressing the key and
/// the microphone opening (WebRTC sends 10-20 ms audio frames anyway).
const POLL: Duration = Duration::from_millis(5);

pub const DEFAULT_KEY: &str = "F8";

#[derive(Clone)]
pub struct PttKey(Arc<Mutex<Option<Hotkey>>>);

impl Default for PttKey {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(DEFAULT_KEY.parse().ok())))
    }
}

impl PttKey {
    pub fn set(&self, key: Option<Hotkey>) {
        *self.0.lock().unwrap() = key;
    }

    fn is_down(&self) -> bool {
        self.0.lock().unwrap().as_ref().is_some_and(Hotkey::is_down)
    }
}

/// Polls the key for the lifetime of the app and drives push-to-talk.
pub fn start(app: AppHandle, key: PttKey) {
    let spawned = std::thread::Builder::new()
        .name("push-to-talk".into())
        .spawn(move || {
            let mut was_down = false;
            loop {
                std::thread::sleep(POLL);
                let down = key.is_down();
                if down != was_down {
                    was_down = down;
                    tracing::debug!(down, "push-to-talk key");
                    app.state::<AppState>().voice.set_ptt(down);
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!("cannot start push-to-talk: {e}");
    }
}

/// Waits for the next key or mouse button (with modifiers) the user presses.
pub fn capture(timeout: Duration) -> Option<Hotkey> {
    let deadline = Instant::now() + timeout;
    // Let go of whatever is still held (e.g. the key that triggered this).
    while Hotkey::capture().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    while Instant::now() < deadline {
        if let Some(key) = Hotkey::capture() {
            return Some(key);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}
