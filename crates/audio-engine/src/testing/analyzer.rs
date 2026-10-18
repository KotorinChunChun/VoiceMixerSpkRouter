//! Pure analysis helpers. Frequency measurement uses a home-grown Goertzel
//! implementation so tests do not pull in an FFT crate.

const MIN_LINEAR: f32 = 1.0e-12;

pub fn peak_dbfs(samples: &[f32]) -> f32 {
    let peak = samples
        .iter()
        .fold(0.0_f32, |acc, sample| acc.max(sample.abs()));
    linear_to_db(peak)
}

pub fn rms_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|sample| {
            let v = f64::from(*sample);
            v * v
        })
        .sum();
    linear_to_db((sum_sq / samples.len() as f64).sqrt() as f32)
}

/// Peak amplitude of a sinusoid at `freq_hz`, in dBFS. `samples` must be a
/// single channel (callers deinterleave or generate mono).
pub fn tone_level_dbfs(samples: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    linear_to_db(goertzel_peak(samples, sample_rate, freq_hz))
}

pub fn snr_db(signal: &[f32], reference_freq_hz: f32, sample_rate: u32) -> f32 {
    if signal.is_empty() {
        return f32::NEG_INFINITY;
    }
    let peak = goertzel_peak(signal, sample_rate, reference_freq_hz);
    let signal_power = (f64::from(peak) * f64::from(peak)) / 2.0;
    let total_power: f64 = signal
        .iter()
        .map(|sample| {
            let v = f64::from(*sample);
            v * v
        })
        .sum::<f64>()
        / signal.len() as f64;
    let noise_power = (total_power - signal_power).max(f64::from(MIN_LINEAR));
    (10.0 * (signal_power / noise_power).log10()) as f32
}

pub fn find_clicks(samples: &[f32], threshold: f32) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut index = 0;
    while index < samples.len() {
        if samples[index].abs() >= threshold {
            positions.push(index);
            index += 1;
            while index < samples.len() && samples[index].abs() >= threshold {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    positions
}

pub fn max_sample_step(samples: &[f32]) -> f32 {
    samples
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0_f32, f32::max)
}

/// Time in milliseconds for a 1 ms peak envelope to move from `from_db` to
/// `to_db`. Returns `None` when either level is never reached.
pub fn envelope_ms(samples: &[f32], sample_rate: u32, from_db: f32, to_db: f32) -> Option<f32> {
    let window = (sample_rate.max(1) as usize / 1_000).max(1);
    let envelope: Vec<f32> = samples
        .chunks(window)
        .map(|chunk| {
            let peak = chunk
                .iter()
                .fold(0.0_f32, |acc, sample| acc.max(sample.abs()));
            linear_to_db(peak)
        })
        .collect();
    let decaying = from_db > to_db;
    let start = envelope.iter().position(|level| {
        if decaying {
            *level < from_db - 0.5
        } else {
            *level > from_db + 0.5
        }
    })?;
    let end = envelope.iter().skip(start).position(|level| {
        if decaying {
            *level <= to_db + 0.5
        } else {
            *level >= to_db - 0.5
        }
    })? + start;
    Some((end.saturating_sub(start) as f32) * (window as f32) * 1_000.0 / sample_rate as f32)
}

fn goertzel_peak(samples: &[f32], sample_rate: u32, freq_hz: f32) -> f32 {
    let n = samples.len();
    if n == 0 || sample_rate == 0 {
        return 0.0;
    }
    let omega = 2.0 * std::f64::consts::PI * f64::from(freq_hz) / f64::from(sample_rate);
    let coeff = 2.0 * omega.cos();
    let mut s1 = 0.0_f64;
    let mut s2 = 0.0_f64;
    for &sample in samples {
        let s0 = f64::from(sample) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
    // Convert Goertzel power to cosine peak amplitude.
    ((power.sqrt() * 2.0) / n as f64) as f32
}

fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.max(MIN_LINEAR).log10()
}

#[cfg(test)]
mod tests {
    use super::{envelope_ms, snr_db};
    use crate::testing::signal_gen::{db_to_linear, generate, SignalKind, SignalSpec};
    use proptest::prelude::*;

    #[test]
    fn t00_u_04_envelope_ms_known_decay() {
        let sample_rate = 48_000u32;
        let hold_ms = 20u32;
        let fade_ms = 10u32;
        let hold_frames = (sample_rate * hold_ms / 1_000) as usize;
        let fade_frames = (sample_rate * fade_ms / 1_000) as usize;
        let from_db = -6.0_f32;
        let to_db = -40.0_f32;
        let from_lin = db_to_linear(from_db);
        let to_lin = db_to_linear(to_db);
        let mut samples = vec![from_lin; hold_frames];
        for i in 0..fade_frames {
            let t = i as f32 / fade_frames.max(1) as f32;
            let db = from_db + (to_db - from_db) * t;
            samples.push(db_to_linear(db));
        }
        samples.extend(std::iter::repeat(to_lin).take(sample_rate as usize / 50));
        let measured = envelope_ms(&samples, sample_rate, from_db, to_db).expect("decay envelope");
        assert!(
            (measured - fade_ms as f32).abs() <= 1.0,
            "measured {measured} ms, expected {fade_ms} ms"
        );
    }

    #[test]
    fn t00_u_05_snr_db_known_mix() {
        let spec = SignalSpec {
            kind: SignalKind::Sine,
            frequency: 1_000.0,
            amplitude_db: -6.0,
            duration_ms: 1_000,
            sample_rate: 48_000,
            channels: 1,
            seed: 1,
            click_interval_ms: 100,
        };
        let sine = generate(&spec);
        let mut noise_spec = spec.clone();
        noise_spec.kind = SignalKind::Noise;
        noise_spec.amplitude_db = 0.0;
        let unit_noise = generate(&noise_spec);
        let sine_rms = db_to_linear(spec.amplitude_db) / std::f32::consts::SQRT_2;
        let target_snr_db = 20.0_f32;
        let noise_rms = sine_rms / 10.0_f32.powf(target_snr_db / 20.0);
        let noise_unit_rms = {
            let sum_sq: f64 = unit_noise
                .iter()
                .map(|s| f64::from(*s) * f64::from(*s))
                .sum();
            (sum_sq / unit_noise.len() as f64).sqrt() as f32
        };
        let scale = noise_rms / noise_unit_rms.max(1.0e-12);
        let mixed: Vec<f32> = sine
            .iter()
            .zip(unit_noise.iter())
            .map(|(s, n)| s + n * scale)
            .collect();
        let measured = snr_db(&mixed, spec.frequency, spec.sample_rate);
        assert!(
            (measured - target_snr_db).abs() <= 1.0,
            "measured {measured} dB, expected {target_snr_db} dB"
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]
        #[test]
        fn t00_p_01_generate_stays_finite_and_bounded(
            kind_idx in 0u8..6,
            frequency in 20.0f32..8_000.0,
            amplitude_db in -60.0f32..=0.0,
            duration_ms in 1u32..40,
            sample_rate in prop_sample_rate(),
            channels in 1u16..=2,
            seed in 0u64..1_000,
            click_interval_ms in 5u32..40,
        ) {
            let kind = match kind_idx {
                0 => SignalKind::Sine,
                1 => SignalKind::Sweep,
                2 => SignalKind::Click,
                3 => SignalKind::Noise,
                4 => SignalKind::Silence,
                _ => SignalKind::SpeechLike,
            };
            let spec = SignalSpec {
                kind,
                frequency,
                amplitude_db,
                duration_ms,
                sample_rate,
                channels,
                seed,
                click_interval_ms,
            };
            let samples = generate(&spec);
            prop_assert_eq!(samples.len(), spec.sample_count());
            for sample in &samples {
                prop_assert!(sample.is_finite());
                prop_assert!(*sample <= 1.0 + 1.0e-6);
                prop_assert!(*sample >= -1.0 - 1.0e-6);
            }
        }
    }

    fn prop_sample_rate() -> impl proptest::strategy::Strategy<Value = u32> {
        proptest::sample::select(vec![8_000, 16_000, 44_100, 48_000])
    }
}
