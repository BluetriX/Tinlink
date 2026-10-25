//! Microphone and speaker selection.
//!
//! The platform audio device module only lives while a `PlatformAudio`
//! handle exists, so the chosen devices are remembered here and applied
//! whenever a session creates one (and immediately during a call).

use std::sync::Mutex;

use anyhow::{Result, anyhow};
use livekit::{PlatformAudio, PlayoutDeviceId, RecordingDeviceId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioDevices {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeviceKind {
    Input,
    Output,
}

/// Selected devices; `None` means the system default.
#[derive(Default)]
pub struct AudioPrefs {
    input: Mutex<Option<String>>,
    output: Mutex<Option<String>>,
}

impl AudioPrefs {
    pub fn set(&self, kind: DeviceKind, id: Option<String>) {
        let slot = match kind {
            DeviceKind::Input => &self.input,
            DeviceKind::Output => &self.output,
        };
        *slot.lock().unwrap() = id;
    }

    /// Applies the remembered devices, e.g. right after a session created
    /// its `PlatformAudio`. Failures are logged; the default device stays.
    pub fn apply(&self, audio: &PlatformAudio) {
        for kind in [DeviceKind::Input, DeviceKind::Output] {
            let id = match kind {
                DeviceKind::Input => self.input.lock().unwrap().clone(),
                DeviceKind::Output => self.output.lock().unwrap().clone(),
            };
            if let Some(id) = id
                && let Err(e) = switch(audio, kind, &id)
            {
                tracing::warn!(?kind, %id, "cannot select audio device: {e:#}");
            }
        }
    }
}

impl AudioPrefs {
    /// Name of the selected microphone, `None` for the system default.
    pub fn input_name(&self, audio: &PlatformAudio) -> Option<String> {
        let id = self.input.lock().unwrap().clone()?;
        audio
            .recording_devices()
            .find(|d| d.id.as_str() == id)
            .map(|d| d.name)
    }
}

pub fn list(audio: &PlatformAudio) -> AudioDevices {
    AudioDevices {
        inputs: audio
            .recording_devices()
            .map(|d| AudioDevice {
                id: d.id.as_str().to_owned(),
                name: d.name,
            })
            .collect(),
        outputs: audio
            .playout_devices()
            .map(|d| AudioDevice {
                id: d.id.as_str().to_owned(),
                name: d.name,
            })
            .collect(),
    }
}

/// The display name of a device id.
pub fn device_name(audio: &PlatformAudio, kind: DeviceKind, id: &str) -> Option<String> {
    match kind {
        DeviceKind::Input => audio
            .recording_devices()
            .find(|d| d.id.as_str() == id)
            .map(|d| d.name),
        DeviceKind::Output => audio
            .playout_devices()
            .find(|d| d.id.as_str() == id)
            .map(|d| d.name),
    }
}

/// Switches the device, also while audio is running.
pub fn switch(audio: &PlatformAudio, kind: DeviceKind, id: &str) -> Result<()> {
    match kind {
        DeviceKind::Input => {
            audio.switch_recording_device(&RecordingDeviceId::from_unchecked_guid(id))
        }
        DeviceKind::Output => {
            audio.switch_playout_device(&PlayoutDeviceId::from_unchecked_guid(id))
        }
    }
    .map_err(|e| anyhow!("{e}"))
}
