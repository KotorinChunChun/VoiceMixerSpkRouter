//! Deterministic test-signal generators. The same `SignalSpec` always yields
//! identical PCM bits.

use std::f64::consts::PI as PI64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    Sine,
    Sweep,
    Click,
    Noise,
    Silence,
    SpeechLike,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignalSpec {
    pub kind: SignalKind,
    pub frequency: f32,
    pub amplitude_db: f32,
    pub duration_ms: u32,
    pub sample_rate: u32,
    pub channels: u16,
    pub seed: u64,
    pub click_interval_ms: u32,
}

impl Default for SignalSpec {
    fn default() -> Self {
        Self {
            kind: SignalKind::Sine,
            frequency: 1_000.0,
            amplitude_db: -6.0,
            duration_ms: 1_000,
            sample_rate: 48_000,
            channels: 2,
            seed: 42,
            click_interval_ms: 100,
        }
    }
}

impl SignalSpec {
    pub fn frame_count(&self) -> usize {
        let sr = u64::from(self.sample_rate.max(1));
        (sr * u64::from(self.duration_ms) / 1_000) as usize
    }

    pub fn sample_count(&self) -> usize {
        self.frame_count() * usize::from(self.channels.max(1))
    }
}

pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Interleaved PCM in `[-1.0, 1.0]` (not hard-clipped; callers should keep
/// `amplitude_db` at or below 0 dBFS for the property-test bound).
pub fn generate(spec: &SignalSpec) -> Vec<f32> {
    let channels = usize::from(spec.channels.max(1));
    let frames = spec.frame_count();
    let mono = generate_mono(spec, frames);
    if channels == 1 {
        return mono;
    }
    let mut out = Vec::with_capacity(frames * channels);
    for sample in mono {
        for _ in 0..channels {
            out.push(sample);
        }
    }
    out
}

fn generate_mono(spec: &SignalSpec, frames: usize) -> Vec<f32> {
    let amp = db_to_linear(spec.amplitude_db);
    let sr = spec.sample_rate.max(1) as f64;
    match spec.kind {
        SignalKind::Silence => vec![0.0; frames],
        SignalKind::Sine => {
            let step = 2.0 * PI64 * f64::from(spec.frequency.max(0.0)) / sr;
            (0..frames)
                .map(|i| (step * i as f64).sin() as f32 * amp)
                .collect()
        }
        SignalKind::Sweep => {
            let nyquist = sr * 0.45;
            let f0 = 20.0_f64.min(nyquist);
            let f1 = 20_000.0_f64.min(nyquist).max(f0);
            let denom = (frames.saturating_sub(1)).max(1) as f64;
            let mut phase = 0.0_f64;
            (0..frames)
                .map(|i| {
                    let t = i as f64 / denom;
                    let freq = f0 * (f1 / f0).powf(t);
                    phase += 2.0 * PI64 * freq / sr;
                    phase.sin() as f32 * amp
                })
                .collect()
        }
        SignalKind::Click => {
            let mut samples = vec![0.0; frames];
            let interval = ((u64::from(spec.sample_rate.max(1))
                * u64::from(spec.click_interval_ms.max(1)))
                / 1_000)
                .max(1) as usize;
            let mut frame = 0;
            while frame < frames {
                samples[frame] = amp;
                frame += interval;
            }
            samples
        }
        SignalKind::Noise => {
            let mut state = mix_seed(spec.seed);
            (0..frames)
                .map(|_| xorshift_f32(&mut state) * amp)
                .collect()
        }
        SignalKind::SpeechLike => {
            let carrier_step = 2.0 * PI64 * f64::from(spec.frequency.max(0.0)) / sr;
            let mod_step = 2.0 * PI64 * 4.0 / sr;
            (0..frames)
                .map(|i| {
                    let t = i as f64;
                    let env = 0.5 + 0.5 * (mod_step * t).sin();
                    ((carrier_step * t).sin() * env) as f32 * amp
                })
                .collect()
        }
    }
}

fn mix_seed(seed: u64) -> u64 {
    seed ^ 0x9E37_79B9_7F4A_7C15
}

fn xorshift_f32(state: &mut u64) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    // 24-bit mantissa in [-1, 1)
    let bits = (x >> 40) as u32 & 0x00FF_FFFF;
    (bits as f32 / (1u32 << 23) as f32) - 1.0
}

#[cfg(test)]
mod tests {
    use super::{generate, SignalKind, SignalSpec};
    use crate::testing::analyzer::{find_clicks, tone_level_dbfs};

    fn mono_sine(amplitude_db: f32) -> SignalSpec {
        SignalSpec {
            kind: SignalKind::Sine,
            frequency: 1_000.0,
            amplitude_db,
            duration_ms: 1_000,
            sample_rate: 48_000,
            channels: 1,
            seed: 42,
            click_interval_ms: 100,
        }
    }

    #[test]
    fn t00_u_01_sine_tone_level_matches_amplitude() {
        let spec = mono_sine(-6.0);
        let samples = generate(&spec);
        let measured = tone_level_dbfs(&samples, spec.sample_rate, spec.frequency);
        assert!(
            (measured - spec.amplitude_db).abs() <= 0.1,
            "measured {measured} dB, expected {}",
            spec.amplitude_db
        );
    }

    #[test]
    fn t00_u_02_click_positions_match_interval() {
        let spec = SignalSpec {
            kind: SignalKind::Click,
            amplitude_db: -6.0,
            duration_ms: 500,
            sample_rate: 48_000,
            channels: 1,
            click_interval_ms: 100,
            ..SignalSpec::default()
        };
        let samples = generate(&spec);
        let clicks = find_clicks(&samples, 0.1);
        let interval = 4_800usize;
        let expected: Vec<usize> = (0..5).map(|i| i * interval).collect();
        assert_eq!(clicks.len(), expected.len());
        for (actual, want) in clicks.iter().zip(expected) {
            let delta = (*actual as i64 - want as i64).unsigned_abs();
            assert!(delta <= 1, "click at {actual}, expected {want}");
        }
    }

    #[test]
    fn t00_u_03_noise_is_deterministic() {
        let spec = SignalSpec {
            kind: SignalKind::Noise,
            duration_ms: 200,
            channels: 2,
            seed: 7,
            ..SignalSpec::default()
        };
        let a = generate(&spec);
        let b = generate(&spec);
        assert_eq!(a, b);
        let mut other = spec.clone();
        other.seed = 8;
        assert_ne!(a, generate(&other));
    }
}
