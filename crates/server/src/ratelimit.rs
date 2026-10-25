//! Simple in-memory limiter against password guessing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failed attempts allowed per key within `WINDOW`.
pub const MAX_FAILURES: u32 = 10;
pub const WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub struct LoginLimiter {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl LoginLimiter {
    /// Returns how long the key is still blocked, if it is.
    pub fn blocked_for(&self, key: &str) -> Option<Duration> {
        let map = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let (count, since) = map.get(key)?;
        let elapsed = since.elapsed();
        (*count >= MAX_FAILURES && elapsed < WINDOW).then(|| WINDOW - elapsed)
    }

    pub fn record_failure(&self, key: &str) {
        let mut map = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        // Drop stale entries so the map cannot grow without bound.
        if map.len() > 10_000 {
            map.retain(|_, (_, since)| since.elapsed() < WINDOW);
        }
        let entry = map.entry(key.to_owned()).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    pub fn reset(&self, key: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max_failures() {
        let limiter = LoginLimiter::default();
        for _ in 0..MAX_FAILURES - 1 {
            limiter.record_failure("erik");
        }
        assert!(limiter.blocked_for("erik").is_none());
        limiter.record_failure("erik");
        assert!(limiter.blocked_for("erik").is_some());
        assert!(limiter.blocked_for("other").is_none());
        limiter.reset("erik");
        assert!(limiter.blocked_for("erik").is_none());
    }
}
