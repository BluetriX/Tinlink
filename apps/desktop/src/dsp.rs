//! Small audio building blocks for what this client hears: speaking
//! detection, a compressor, a jitter buffer per participant and a resampler
//! to the output device's rate. Pure code without devices, so it is tested
//! on every platform.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Audio at least this loud counts as someone speaking (dBFS). Low enough
/// that audible background noise lights the dot too: it shows what is sent.
pub const AUDIBLE_DB: f32 = -60.0;
/// The speaking dot stays on this long after the last loud moment.
pub const SPEAKING_HOLD: Duration = Duration::from_millis(250);

/// Sample rate of the decoded remote audio.
pub const RATE: u32 = 48_000;

pub fn rms_db(samples: impl Iterator<Item = f32>) -> f32 {
    let (sum, n) = samples.fold((0.0f64, 0usize), |(s, n), x| (s + f64::from(x * x), n + 1));
    if n == 0 || sum <= 0.0 {
        return -120.0;
    }
    (10.0 * (sum / n as f64).log10()) as f32
}

/// When someone was last audible.
#[derive(Debug, Default)]
pub struct Audible {
    last_loud: Option<Instant>,
}

impl Audible {
    pub fn record(&mut self, now: Instant, level_db: f32) {
        if level_db >= AUDIBLE_DB {
            self.last_loud = Some(now);
        }
    }

    pub fn speaking(&self, now: Instant) -> bool {
        self.last_loud
            .is_some_and(|t| now.saturating_duration_since(t) <= SPEAKING_HOLD)
    }
}

/// Compressor strength for one participant; the values match the web client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompressorPreset {
    #[default]
    Off,
    Light,
    Strong,
}

struct CompressorParams {
    threshold_db: f32,
    knee_db: f32,
    ratio: f32,
    attack_s: f32,
    release_s: f32,
}

impl CompressorPreset {
    fn params(self) -> Option<CompressorParams> {
        match self {
            Self::Off => None,
            Self::Light => Some(CompressorParams {
                threshold_db: -30.0,
                knee_db: 10.0,
                ratio: 3.0,
                attack_s: 0.005,
                release_s: 0.2,
            }),
            Self::Strong => Some(CompressorParams {
                threshold_db: -45.0,
                knee_db: 6.0,
                ratio: 8.0,
                attack_s: 0.003,
                release_s: 0.25,
            }),
        }
    }
}

/// Feed-forward compressor with a soft knee and automatic makeup gain
/// (like the browser's DynamicsCompressorNode): loud speech is turned down,
/// quiet speech comes up.
#[derive(Debug, Default)]
pub struct Compressor {
    preset: CompressorPreset,
    /// Smoothed gain reduction in dB (0 or negative).
    reduction_db: f32,
}

impl Compressor {
    pub fn set_preset(&mut self, preset: CompressorPreset) {
        if preset != self.preset {
            self.preset = preset;
            self.reduction_db = 0.0;
        }
    }

    /// Static gain reduction (dB, <= 0) for an input level.
    fn curve(p: &CompressorParams, level_db: f32) -> f32 {
        let over = level_db - p.threshold_db;
        let slope = 1.0 / p.ratio - 1.0;
        if 2.0 * over <= -p.knee_db {
            0.0
        } else if 2.0 * over.abs() <= p.knee_db {
            slope * (over + p.knee_db / 2.0).powi(2) / (2.0 * p.knee_db)
        } else {
            slope * over
        }
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        let Some(p) = self.preset.params() else {
            return;
        };
        let rate = RATE as f32;
        let attack = (-1.0 / (p.attack_s * rate)).exp();
        let release = (-1.0 / (p.release_s * rate)).exp();
        // Brings a full scale signal back to roughly where it was.
        let makeup_db = -0.6 * Self::curve(&p, 0.0);
        for s in samples {
            let level_db = 20.0 * (s.abs() + 1e-9).log10();
            let target = Self::curve(&p, level_db);
            let coeff = if target < self.reduction_db {
                attack
            } else {
                release
            };
            self.reduction_db = coeff * self.reduction_db + (1.0 - coeff) * target;
            *s *= 10f32.powf((self.reduction_db + makeup_db) / 20.0);
        }
    }
}

/// Decoded audio of one participant waiting for the output device. Keeps
/// latency bounded: starts playing after `PRIME` samples, drops the oldest
/// samples beyond `MAX`, and primes again after running dry.
#[derive(Debug, Default)]
pub struct JitterBuffer {
    samples: VecDeque<f32>,
    playing: bool,
}

impl JitterBuffer {
    /// 20 ms at 48 kHz.
    pub const PRIME: usize = 960;
    /// 60 ms at 48 kHz.
    pub const MAX: usize = 2880;

    pub fn push(&mut self, samples: &[f32]) {
        self.samples.extend(samples);
        let excess = self.samples.len().saturating_sub(Self::MAX);
        self.samples.drain(..excess);
    }

    /// Adds up to `out.len()` samples onto `out`.
    pub fn mix_into(&mut self, out: &mut [f32]) {
        if !self.playing {
            if self.samples.len() < Self::PRIME {
                return;
            }
            self.playing = true;
        }
        let n = out.len().min(self.samples.len());
        for (o, s) in out.iter_mut().zip(self.samples.drain(..n)) {
            *o += s;
        }
        if n < out.len() {
            self.playing = false;
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.playing = false;
    }
}

/// Linear resampler from 48 kHz to the output rate. Good enough for speech
/// and free of latency.
#[derive(Debug)]
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    pos: f64,
    prev: f32,
    next: f32,
}

impl Resampler {
    pub fn new(output_rate: u32) -> Self {
        Self {
            step: f64::from(RATE) / f64::from(output_rate.max(1)),
            // Pull the first input sample right away.
            pos: 1.0,
            prev: 0.0,
            next: 0.0,
        }
    }

    /// One output sample; `pull` delivers the next 48 kHz input sample.
    pub fn resample(&mut self, mut pull: impl FnMut() -> f32) -> f32 {
        while self.pos >= 1.0 {
            self.prev = self.next;
            self.next = pull();
            self.pos -= 1.0;
        }
        let out = self.prev + (self.next - self.prev) * self.pos as f32;
        self.pos += self.step;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_full_scale_sine_is_about_minus_3_db() {
        let sine = (0..480).map(|i| (i as f32 / 480.0 * std::f32::consts::TAU * 10.0).sin());
        let db = rms_db(sine);
        assert!((-3.1..-2.9).contains(&db), "{db}");
        assert_eq!(rms_db(std::iter::empty()), -120.0);
    }

    #[test]
    fn speaking_holds_briefly_after_sound() {
        let t0 = Instant::now();
        let mut a = Audible::default();
        assert!(!a.speaking(t0));
        a.record(t0, -70.0);
        assert!(!a.speaking(t0));
        a.record(t0, -55.0);
        assert!(a.speaking(t0 + Duration::from_millis(200)));
        assert!(!a.speaking(t0 + Duration::from_millis(300)));
    }

    #[test]
    fn compressor_evens_out_levels() {
        let sine = |amp: f32| -> Vec<f32> {
            (0..RATE as usize / 2)
                .map(|i| amp * (i as f32 / 48.0 * std::f32::consts::TAU).sin())
                .collect()
        };
        let level = |preset, amp| {
            let mut c = Compressor::default();
            c.set_preset(preset);
            let mut s = sine(amp);
            c.process(&mut s);
            // Skip the attack phase.
            rms_db(s[s.len() / 2..].iter().copied())
        };
        let (loud, quiet) = (0.5, 0.01);
        let spread_off = level(CompressorPreset::Off, loud) - level(CompressorPreset::Off, quiet);
        let spread_light =
            level(CompressorPreset::Light, loud) - level(CompressorPreset::Light, quiet);
        let spread_strong =
            level(CompressorPreset::Strong, loud) - level(CompressorPreset::Strong, quiet);
        assert!((spread_off - 34.0).abs() < 0.5, "{spread_off}");
        assert!(spread_light < spread_off - 8.0, "{spread_light}");
        assert!(spread_strong < spread_light - 5.0, "{spread_strong}");
        // Makeup gain keeps loud speech at a normal level.
        let loud_strong = level(CompressorPreset::Strong, loud);
        assert!((-25.0..-3.0).contains(&loud_strong), "{loud_strong}");
    }

    #[test]
    fn jitter_buffer_primes_caps_and_reprimes() {
        let mut b = JitterBuffer::default();
        let mut out = [0.0; 480];
        b.push(&[1.0; 480]);
        b.mix_into(&mut out);
        assert_eq!(out[0], 0.0, "not primed yet");
        b.push(&[1.0; 480]);
        b.mix_into(&mut out);
        assert!(out.iter().all(|&s| s == 1.0));
        // 480 left; asking for 960 runs dry and primes again.
        let mut out = [0.0; 960];
        b.mix_into(&mut out);
        assert_eq!(out[479], 1.0);
        assert_eq!(out[480], 0.0);
        b.push(&[1.0; 480]);
        let mut out = [0.0; 480];
        b.mix_into(&mut out);
        assert_eq!(out[0], 0.0, "priming again");
        b.push(&[2.0; 10_000]);
        assert_eq!(b.samples.len(), JitterBuffer::MAX);
    }

    #[test]
    fn resampler_keeps_the_rate() {
        let mut r = Resampler::new(44_100);
        let mut pulled = 0usize;
        for _ in 0..44_100 {
            r.resample(|| {
                pulled += 1;
                0.5
            });
        }
        assert!((47_990..48_010).contains(&pulled), "{pulled}");
        let mut same = Resampler::new(48_000);
        let mut i = 0.0;
        let out: Vec<f32> = (0..5)
            .map(|_| {
                same.resample(|| {
                    i += 1.0;
                    i
                })
            })
            .collect();
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 4.0]);
    }
}
