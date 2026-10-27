//! What this client hears from each remote participant.
//!
//! - Every remote microphone is tapped after decoding, so the speaking dot
//!   follows the sound itself. LiveKit's active speaker flags arrive from the
//!   server several hundred milliseconds late and ignore quiet sound such as
//!   background noise.
//! - WebRTC's playout has no volume per track. A participant whose volume
//!   or compressor the user changed is therefore muted there and played
//!   through an own output stream instead, with the volume and compressor
//!   applied. Everyone else stays on WebRTC's playout, which also feeds the
//!   echo canceller and adds no latency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures_util::StreamExt;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::prelude::RtcAudioTrack;
use serde::Deserialize;

use crate::dsp::{self, Audible, Compressor, CompressorPreset, JitterBuffer, Resampler};

/// Volume and compressor for one participant, for this client only.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenPrefs {
    /// 1.0 = unchanged, up to 2.0.
    pub volume: f32,
    pub compressor: CompressorPreset,
}

impl Default for ListenPrefs {
    fn default() -> Self {
        Self {
            volume: 1.0,
            compressor: CompressorPreset::Off,
        }
    }
}

impl ListenPrefs {
    /// Whether WebRTC's playout cannot do it alone.
    fn needs_own_playout(&self) -> bool {
        self.compressor != CompressorPreset::Off || (self.volume - 1.0).abs() > 0.005
    }
}

/// When each participant (by identity) was last audible; read by the
/// participant list and the camera grid.
#[derive(Default)]
pub struct Speakers(Mutex<HashMap<String, Audible>>);

impl Speakers {
    pub fn record(&self, identity: &str, level_db: f32) {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap();
        match map.get_mut(identity) {
            Some(audible) => audible.record(now, level_db),
            None if level_db >= dsp::AUDIBLE_DB => {
                let mut audible = Audible::default();
                audible.record(now, level_db);
                map.insert(identity.to_owned(), audible);
            }
            None => {}
        }
    }

    pub fn speaking(&self, identity: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(identity)
            .is_some_and(|a| a.speaking(Instant::now()))
    }

    fn forget(&self, identity: &str) {
        self.0.lock().unwrap().remove(identity);
    }
}

/// One remote microphone.
struct Source {
    track: RtcAudioTrack,
    state: Mutex<SourceState>,
}

#[derive(Default)]
struct SourceState {
    prefs: ListenPrefs,
    muted: bool,
    compressor: Compressor,
    buffer: JitterBuffer,
}

impl Source {
    /// Mutes the track in WebRTC's playout when muted or played by us.
    fn apply(&self) {
        let mut state = self.state.lock().unwrap();
        let own = !state.muted && state.prefs.needs_own_playout();
        let preset = state.prefs.compressor;
        state.compressor.set_preset(preset);
        if !own {
            state.buffer.clear();
        }
        self.track.set_enabled(!state.muted && !own);
    }

    fn own_playout(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.muted && state.prefs.needs_own_playout()
    }
}

struct Tap {
    source: Arc<Source>,
    task: tokio::task::JoinHandle<()>,
}

/// Remote audio of one session.
pub struct RemoteAudio {
    speakers: Arc<Speakers>,
    taps: Mutex<HashMap<String, Tap>>,
    sources: Arc<Mutex<Vec<Arc<Source>>>>,
    output: Mutex<Option<Output>>,
    output_device: Mutex<Option<String>>,
}

impl RemoteAudio {
    /// `output_device`: name of the speaker WebRTC plays on (None: default).
    pub fn new(speakers: Arc<Speakers>, output_device: Option<String>) -> Self {
        Self {
            speakers,
            taps: Mutex::default(),
            sources: Arc::default(),
            output: Mutex::default(),
            output_device: Mutex::new(output_device),
        }
    }

    /// Starts measuring (and, if needed, playing) a subscribed microphone.
    pub fn add(&self, identity: &str, track: RtcAudioTrack, prefs: ListenPrefs, muted: bool) {
        self.remove(identity);
        let source = Arc::new(Source {
            track: track.clone(),
            state: Mutex::new(SourceState {
                prefs,
                muted,
                ..Default::default()
            }),
        });
        source.apply();
        let task = tokio::spawn(tap(
            NativeAudioStream::new(track, dsp::RATE as i32, 1),
            identity.to_owned(),
            source.clone(),
            self.speakers.clone(),
        ));
        self.sources.lock().unwrap().push(source.clone());
        self.taps
            .lock()
            .unwrap()
            .insert(identity.to_owned(), Tap { source, task });
        self.update_output();
    }

    pub fn remove(&self, identity: &str) {
        let Some(tap) = self.taps.lock().unwrap().remove(identity) else {
            return;
        };
        tap.task.abort();
        self.sources
            .lock()
            .unwrap()
            .retain(|s| !Arc::ptr_eq(s, &tap.source));
        self.speakers.forget(identity);
        self.update_output();
    }

    pub fn set_prefs(&self, identity: &str, prefs: ListenPrefs) {
        self.change(identity, |state| state.prefs = prefs);
    }

    pub fn set_muted(&self, identity: &str, muted: bool) {
        self.change(identity, |state| state.muted = muted);
    }

    fn change(&self, identity: &str, f: impl FnOnce(&mut SourceState)) {
        let Some(source) = self
            .taps
            .lock()
            .unwrap()
            .get(identity)
            .map(|t| t.source.clone())
        else {
            return;
        };
        f(&mut source.state.lock().unwrap());
        source.apply();
        self.update_output();
    }

    /// Plays on another speaker from now on.
    pub fn set_output_device(&self, name: Option<String>) {
        *self.output_device.lock().unwrap() = name;
        // Restarted on the new device if it is needed.
        self.output.lock().unwrap().take();
        self.update_output();
    }

    /// Runs the own output stream only while someone needs it.
    fn update_output(&self) {
        let needed = self.sources.lock().unwrap().iter().any(|s| s.own_playout());
        let mut output = self.output.lock().unwrap();
        if needed && output.is_none() {
            let device = self.output_device.lock().unwrap().clone();
            *output = Some(Output::start(device, self.sources.clone()));
        } else if !needed {
            output.take();
        }
    }
}

impl Drop for RemoteAudio {
    fn drop(&mut self) {
        for (_, tap) in self.taps.lock().unwrap().drain() {
            tap.task.abort();
            tap.source.track.set_enabled(true);
        }
    }
}

/// Reads decoded 10 ms frames of one remote microphone.
async fn tap(
    mut stream: NativeAudioStream,
    identity: String,
    source: Arc<Source>,
    speakers: Arc<Speakers>,
) {
    let mut samples = Vec::new();
    while let Some(frame) = stream.next().await {
        samples.clear();
        samples.extend(frame.data.iter().map(|&s| f32::from(s) / 32768.0));
        speakers.record(&identity, dsp::rms_db(samples.iter().copied()));
        let mut state = source.state.lock().unwrap();
        if state.muted || !state.prefs.needs_own_playout() {
            continue;
        }
        state.compressor.process(&mut samples);
        let volume = state.prefs.volume;
        for s in &mut samples {
            *s *= volume;
        }
        state.buffer.push(&samples);
    }
}

/// The own output stream; stops when dropped.
struct Output {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    fn start(device_name: Option<String>, sources: Arc<Mutex<Vec<Arc<Source>>>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("listen-output".into())
                .spawn(move || {
                    if let Err(e) = run_output(device_name, &stop, sources) {
                        tracing::warn!("own audio output unavailable: {e:#}");
                    }
                })
                .ok()
        };
        Self { stop, thread }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn output_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name
        && let Ok(mut devices) = host.output_devices()
        && let Some(device) =
            devices.find(|d| d.description().is_ok_and(|desc| desc.name() == name))
    {
        return Some(device);
    }
    host.default_output_device()
}

/// The sources mixed at 48 kHz, in 10 ms blocks.
struct Blocks {
    sources: Arc<Mutex<Vec<Arc<Source>>>>,
    block: Vec<f32>,
    pos: usize,
}

impl Blocks {
    fn next_sample(&mut self) -> f32 {
        if self.pos >= self.block.len() {
            self.block.clear();
            self.block.resize(dsp::RATE as usize / 100, 0.0);
            for source in self.sources.lock().unwrap().iter() {
                source
                    .state
                    .lock()
                    .unwrap()
                    .buffer
                    .mix_into(&mut self.block);
            }
            self.pos = 0;
        }
        self.pos += 1;
        self.block[self.pos - 1]
    }
}

/// The mix resampled to the output device.
struct Mixer {
    blocks: Blocks,
    resampler: Resampler,
}

impl Mixer {
    /// Fills interleaved output frames with the mix on every channel.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        let Self { blocks, resampler } = self;
        for frame in out.chunks_mut(channels.max(1)) {
            frame.fill(resampler.resample(|| blocks.next_sample()).clamp(-1.0, 1.0));
        }
    }
}

fn run_output(
    device_name: Option<String>,
    stop: &AtomicBool,
    sources: Arc<Mutex<Vec<Arc<Source>>>>,
) -> anyhow::Result<()> {
    let device =
        output_device(device_name.as_deref()).ok_or_else(|| anyhow::anyhow!("no output device"))?;
    let supported = device.default_output_config()?;
    let config = supported.config();
    let channels = usize::from(config.channels);
    let mut mixer = Mixer {
        blocks: Blocks {
            sources,
            block: Vec::new(),
            pos: 0,
        },
        resampler: Resampler::new(config.sample_rate),
    };
    let on_error = |e| tracing::debug!("own audio output: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &_| mixer.fill(data, channels),
            on_error,
            None,
        )?,
        cpal::SampleFormat::I16 => {
            let mut scratch = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [i16], _: &_| {
                    scratch.resize(data.len(), 0.0);
                    mixer.fill(&mut scratch, channels);
                    for (d, s) in data.iter_mut().zip(&scratch) {
                        *d = (s * 32767.0) as i16;
                    }
                },
                on_error,
                None,
            )?
        }
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    tracing::debug!(device = ?device_name, "own audio output running");
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}
