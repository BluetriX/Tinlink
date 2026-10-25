//! Notices when someone talks while nothing is transmitted (push-to-talk
//! not held, or muted by a moderator) and asks the UI to play a short beep.
//!
//! WebRTC's audio device module does not expose the microphone level while
//! the track is disabled, so a separate lightweight capture stream (WASAPI
//! shared mode via cpal) on the same microphone measures the level. Only
//! the RMS of each buffer is computed; no audio is kept or sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Continuous talking (short pauses allowed) needed before the beep.
const TALK_DURATION: Duration = Duration::from_secs(10);
/// Pauses up to this long still count as one stretch of talking.
const HANGOVER: Duration = Duration::from_secs(2);
/// Minimum time between two beeps.
const COOLDOWN: Duration = Duration::from_secs(30);
/// Speech must be this much louder than the background noise ...
const SPEECH_ABOVE_NOISE_DB: f32 = 12.0;
/// ... and at least this loud.
const MIN_SPEECH_DB: f32 = -50.0;
/// How fast the noise floor follows a rising level, per update.
const NOISE_RISE_DB: f32 = 0.05;
const POLL: Duration = Duration::from_millis(100);

/// Energy-based voice detection plus the "talking while muted" timing.
#[derive(Debug, Default)]
pub struct Detector {
    /// Starts at the first measured level.
    noise_floor_db: Option<f32>,
    talk_start: Option<Instant>,
    last_speech: Option<Instant>,
    last_beep: Option<Instant>,
}

impl Detector {
    /// Feeds the loudest level (dBFS) since the last update. Returns true
    /// when the user should be reminded that nobody hears them.
    pub fn update(&mut self, now: Instant, level_db: f32, transmitting: bool) -> bool {
        // The floor drops at once and rises slowly, so speech does not pull it up.
        let floor = match self.noise_floor_db {
            Some(floor) if level_db >= floor => floor + NOISE_RISE_DB,
            _ => level_db,
        };
        self.noise_floor_db = Some(floor);
        let speaking = level_db > MIN_SPEECH_DB && level_db > floor + SPEECH_ABOVE_NOISE_DB;

        if transmitting {
            self.talk_start = None;
            self.last_speech = None;
            return false;
        }
        if speaking {
            self.talk_start.get_or_insert(now);
            self.last_speech = Some(now);
        } else if self
            .last_speech
            .is_none_or(|t| now.duration_since(t) > HANGOVER)
        {
            self.talk_start = None;
        }

        let talked_long_enough = self
            .talk_start
            .is_some_and(|t| now.duration_since(t) >= TALK_DURATION);
        let cooled_down = self
            .last_beep
            .is_none_or(|t| now.duration_since(t) >= COOLDOWN);
        if talked_long_enough && cooled_down {
            self.last_beep = Some(now);
            self.talk_start = None;
            return true;
        }
        false
    }
}

fn rms_db(samples: impl Iterator<Item = f32>) -> f32 {
    let (sum, n) = samples.fold((0.0f64, 0usize), |(s, n), x| (s + f64::from(x * x), n + 1));
    if n == 0 || sum <= 0.0 {
        return -120.0;
    }
    (10.0 * (sum / n as f64).log10()) as f32
}

/// Runs the level meter while it lives.
pub struct MutedSpeechMonitor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MutedSpeechMonitor {
    /// `device_name`: the microphone WebRTC uses (None: system default).
    /// `transmitting` is polled; `on_hint` runs when the beep is due.
    pub fn start(
        device_name: Option<String>,
        transmitting: impl Fn() -> bool + Send + 'static,
        on_hint: impl Fn() + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("muted-speech".into())
                .spawn(move || {
                    if let Err(e) = run(device_name, &stop, transmitting, on_hint) {
                        tracing::warn!("muted-speech hint unavailable: {e}");
                    }
                })
                .ok()
        };
        Self { stop, thread }
    }
}

impl Drop for MutedSpeechMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn input_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name
        && let Ok(mut devices) = host.input_devices()
        && let Some(device) =
            devices.find(|d| d.description().is_ok_and(|desc| desc.name() == name))
    {
        return Some(device);
    }
    host.default_input_device()
}

fn run(
    device_name: Option<String>,
    stop: &AtomicBool,
    transmitting: impl Fn() -> bool,
    on_hint: impl Fn(),
) -> anyhow::Result<()> {
    let device =
        input_device(device_name.as_deref()).ok_or_else(|| anyhow::anyhow!("no input device"))?;
    let supported = device.default_input_config()?;
    // Loudest level (dBFS, as f32 bits) since the detector last looked.
    let peak = Arc::new(AtomicU32::new((-120.0f32).to_bits()));
    let record = {
        let peak = peak.clone();
        move |db: f32| {
            let mut old = peak.load(Ordering::Relaxed);
            while db > f32::from_bits(old) {
                match peak.compare_exchange_weak(
                    old,
                    db.to_bits(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(current) => old = current,
                }
            }
        }
    };
    let config = supported.config();
    let on_error = |e| tracing::debug!("level meter: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &_| record(rms_db(data.iter().copied())),
            on_error,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &_| record(rms_db(data.iter().map(|&s| f32::from(s) / 32768.0))),
            on_error,
            None,
        )?,
        other => anyhow::bail!("unsupported sample format {other:?}"),
    };
    stream.play()?;
    tracing::debug!(device = ?device_name, "muted-speech level meter running");

    let mut detector = Detector::default();
    let mut ticks = 0u32;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        let level = f32::from_bits(peak.swap((-120.0f32).to_bits(), Ordering::Relaxed));
        if detector.update(Instant::now(), level, transmitting()) {
            tracing::debug!("talking while not transmitting: hinting");
            on_hint();
        }
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(50) {
            tracing::trace!(level, floor = ?detector.noise_floor_db, "microphone level");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOISE: f32 = -65.0;
    const SPEECH: f32 = -25.0;

    /// Runs the detector at 10 Hz; `level(t)` gives the level at time t (s).
    fn simulate(
        d: &mut Detector,
        start: Instant,
        seconds: f32,
        transmitting: bool,
        level: impl Fn(f32) -> f32,
    ) -> Vec<f32> {
        let mut beeps = Vec::new();
        let steps = (seconds * 10.0) as u32;
        for i in 0..steps {
            let t = i as f32 / 10.0;
            let now = start + Duration::from_secs_f32(t);
            if d.update(now, level(t), transmitting) {
                beeps.push(t);
            }
        }
        beeps
    }

    #[test]
    fn beeps_after_ten_seconds_of_muted_talking() {
        let mut d = Detector::default();
        let start = Instant::now();
        // Quiet room first, then talking with short pauses.
        let beeps = simulate(&mut d, start, 25.0, false, |t| {
            if t < 3.0 || (t % 3.0) > 2.5 {
                NOISE
            } else {
                SPEECH
            }
        });
        assert_eq!(beeps.len(), 1, "{beeps:?}");
        assert!((12.9..14.0).contains(&beeps[0]), "{beeps:?}");
    }

    #[test]
    fn no_beep_while_transmitting() {
        let mut d = Detector::default();
        let beeps = simulate(&mut d, Instant::now(), 30.0, true, |_| SPEECH);
        assert!(beeps.is_empty());
    }

    #[test]
    fn long_pauses_restart_the_count() {
        let mut d = Detector::default();
        // 6 s talking, 4 s silence, repeated: never 10 s in one stretch.
        let beeps = simulate(&mut d, Instant::now(), 40.0, false, |t| {
            if (t % 10.0) < 6.0 { SPEECH } else { NOISE }
        });
        assert!(beeps.is_empty(), "{beeps:?}");
    }

    #[test]
    fn rate_limited_to_once_per_cooldown() {
        let mut d = Detector::default();
        let beeps = simulate(&mut d, Instant::now(), 60.0, false, |t| {
            if t < 1.0 { NOISE } else { SPEECH }
        });
        assert_eq!(beeps.len(), 2, "{beeps:?}");
        assert!(beeps[1] - beeps[0] >= 30.0, "{beeps:?}");
    }

    #[test]
    fn steady_noise_is_not_speech() {
        let mut d = Detector::default();
        // A fan at -40 dBFS: loud, but constant.
        let beeps = simulate(&mut d, Instant::now(), 40.0, false, |_| -40.0);
        assert!(beeps.is_empty(), "{beeps:?}");
    }

    #[test]
    fn rms_of_full_scale_sine_is_about_minus_3_db() {
        let sine = (0..480).map(|i| (i as f32 / 480.0 * std::f32::consts::TAU * 10.0).sin());
        let db = rms_db(sine);
        assert!((-3.1..-2.9).contains(&db), "{db}");
        assert_eq!(rms_db(std::iter::empty()), -120.0);
    }
}
