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
    /// Transmit continuously; WebRTC noise suppression and Opus DTX keep
    /// silence out of the stream.
    VoiceActivation,
}

#[derive(Default)]
pub struct Voice {
    inner: Mutex<Inner>,
}

struct Inner {
    mode: VoiceMode,
    ptt_down: bool,
    /// The published track and the runtime it belongs to: muting spawns
    /// tasks, but the hotkey handler runs on the (non-Tokio) main thread.
    mic: Option<(LocalAudioTrack, tokio::runtime::Handle)>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            mode: VoiceMode::PushToTalk,
            ptt_down: false,
            mic: None,
        }
    }
}

impl Inner {
    fn transmitting(&self) -> bool {
        match self.mode {
            VoiceMode::PushToTalk => self.ptt_down,
            VoiceMode::VoiceActivation => true,
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
        }
    }

    #[test]
    fn push_to_talk_transmits_only_while_held() {
        assert!(!inner(VoiceMode::PushToTalk, false).transmitting());
        assert!(inner(VoiceMode::PushToTalk, true).transmitting());
    }

    #[test]
    fn voice_activation_always_transmits() {
        assert!(inner(VoiceMode::VoiceActivation, false).transmitting());
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
