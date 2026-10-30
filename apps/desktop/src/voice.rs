//! Push-to-talk and voice activation.
//!
//! The microphone track stays published for the whole session; transmitting
//! is toggled by muting the track. A muted track sends nothing (Opus DTX),
//! and unmuting takes effect with the next 10 ms capture frame, so the key
//! press itself adds no latency.

use std::sync::Mutex;

use livekit::prelude::LocalAudioTrack;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VoiceMode {
    /// Transmit only while the push-to-talk key is held.
    PushToTalk,
    /// Transmit while the microphone level is above a threshold (with a
    /// short hangover), see [`crate::mic::Gate`].
    VoiceActivation,
}

#[derive(Default)]
pub struct Voice {
    inner: Mutex<Inner>,
}

/// Default voice activation threshold in dBFS (quiet rooms measure around
/// -80, speech -40 to -20).
pub const DEFAULT_THRESHOLD_DB: f32 = -50.0;

struct Inner {
    mode: VoiceMode,
    ptt_down: bool,
    /// Voice activation gate, driven by the microphone monitor.
    va_open: bool,
    threshold_db: f32,
    /// The user muted the microphone: nothing is sent, whatever the mode.
    self_muted: bool,
    /// The published track and the runtime it belongs to: muting spawns
    /// tasks, but the hotkey handler runs on the (non-Tokio) main thread.
    mic: Option<(LocalAudioTrack, tokio::runtime::Handle)>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            mode: VoiceMode::PushToTalk,
            ptt_down: false,
            va_open: false,
            threshold_db: DEFAULT_THRESHOLD_DB,
            self_muted: false,
            mic: None,
        }
    }
}

impl Inner {
    fn transmitting(&self) -> bool {
        if self.self_muted {
            return false;
        }
        match self.mode {
            VoiceMode::PushToTalk => self.ptt_down,
            VoiceMode::VoiceActivation => self.va_open,
        }
    }

    fn apply(&self) {
        let Some((mic, rt)) = &self.mic else { return };
        let on = self.transmitting();
        if on == mic.is_muted() {
            let _rt = rt.enter();
            if on {
                mic.unmute();
            } else {
                mic.mute();
            }
        }
    }
}

impl Voice {
    pub fn set_mode(&self, mode: VoiceMode) {
        let mut inner = self.inner.lock().unwrap();
        inner.mode = mode;
        inner.apply();
    }

    pub fn set_self_muted(&self, muted: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.self_muted = muted;
        inner.apply();
    }

    pub fn set_ptt(&self, down: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.ptt_down != down {
            inner.ptt_down = down;
            inner.apply();
        }
    }

    /// Attaches the published microphone track (or detaches it with `None`).
    /// Must be called from within the Tokio runtime.
    pub fn set_mic(&self, mic: Option<LocalAudioTrack>) {
        let mut inner = self.inner.lock().unwrap();
        inner.mic = mic.map(|mic| (mic, tokio::runtime::Handle::current()));
        inner.apply();
    }

    pub fn set_va_open(&self, open: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.va_open != open {
            inner.va_open = open;
            inner.apply();
        }
    }

    pub fn set_threshold_db(&self, db: f32) {
        self.inner.lock().unwrap().threshold_db = db.clamp(-90.0, 0.0);
    }

    pub fn threshold_db(&self) -> f32 {
        self.inner.lock().unwrap().threshold_db
    }

    /// Detaches the microphone if it is still `track_id` (another session
    /// may have attached its own in the meantime).
    pub fn release_mic(&self, track_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .mic
            .as_ref()
            .is_some_and(|(mic, _)| mic.rtc_track().id() == track_id)
        {
            inner.mic = None;
        }
    }

    /// Forces the track's enabled state to match the voice state. Needed
    /// after (re)publishing: muting an unpublished track only sets a flag
    /// that the new sender does not pick up, and `mute()` is a no-op while
    /// the flag already says muted.
    pub fn resync(&self) {
        let inner = self.inner.lock().unwrap();
        let Some((mic, rt)) = &inner.mic else { return };
        let on = inner.transmitting();
        let _rt = rt.enter();
        mic.rtc_track().set_enabled(on);
        if on {
            mic.unmute();
        } else {
            mic.mute();
        }
    }

    pub fn transmitting(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.mic.is_some() && inner.transmitting()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner(mode: VoiceMode, ptt_down: bool) -> Inner {
        Inner {
            mode,
            ptt_down,
            mic: None,
            ..Inner::default()
        }
    }

    #[test]
    fn push_to_talk_transmits_only_while_held() {
        assert!(!inner(VoiceMode::PushToTalk, false).transmitting());
        assert!(inner(VoiceMode::PushToTalk, true).transmitting());
    }

    #[test]
    fn voice_activation_follows_the_gate() {
        let mut i = inner(VoiceMode::VoiceActivation, false);
        assert!(!i.transmitting());
        i.va_open = true;
        assert!(i.transmitting());
        // The push-to-talk key does not matter in this mode.
        i.va_open = false;
        i.ptt_down = true;
        assert!(!i.transmitting());
    }

    #[test]
    fn self_mute_overrides_every_mode() {
        let mut i = inner(VoiceMode::PushToTalk, true);
        i.self_muted = true;
        assert!(!i.transmitting());
        i.mode = VoiceMode::VoiceActivation;
        i.va_open = true;
        assert!(!i.transmitting());
        i.self_muted = false;
        assert!(i.transmitting());
    }

    #[test]
    fn threshold_is_clamped() {
        let voice = Voice::default();
        assert_eq!(voice.threshold_db(), DEFAULT_THRESHOLD_DB);
        voice.set_threshold_db(-200.0);
        assert_eq!(voice.threshold_db(), -90.0);
        voice.set_threshold_db(5.0);
        assert_eq!(voice.threshold_db(), 0.0);
    }

    #[test]
    fn nothing_is_transmitted_without_a_microphone() {
        let voice = Voice::default();
        voice.set_mode(VoiceMode::VoiceActivation);
        voice.set_ptt(true);
        assert!(!voice.transmitting());
    }

    #[test]
    fn mode_names_match_the_ui() {
        let mode: VoiceMode = serde_json::from_str("\"pushToTalk\"").unwrap();
        assert_eq!(mode, VoiceMode::PushToTalk);
        let mode: VoiceMode = serde_json::from_str("\"voiceActivation\"").unwrap();
        assert_eq!(mode, VoiceMode::VoiceActivation);
    }
}
