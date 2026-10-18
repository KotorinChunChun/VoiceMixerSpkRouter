use super::{
    compressor::Compressor,
    equalizer::{peak_band_1k_plus6, EqBandParams, Equalizer, EQ_BANDS},
    gate::{GateState, NoiseGate},
    noise_suppressor::NoiseSuppressor,
};
use crate::testing::{
    analyzer::{envelope_ms, max_sample_step, peak_dbfs, snr_db, tone_level_dbfs},
    signal_gen::{db_to_linear, generate, SignalKind, SignalSpec},
};
use proptest::prelude::*;

const SR: u32 = 48_000;
const BLOCK: usize = 480;

fn sine(freq: f32, amp_db: f32, duration_ms: u32) -> Vec<f32> {
    generate(&SignalSpec {
        kind: SignalKind::Sine,
        frequency: freq,
        amplitude_db: amp_db,
        duration_ms,
        sample_rate: SR,
        channels: 1,
        seed: 1,
        click_interval_ms: 100,
    })
}

fn process_blocked(samples: &mut [f32], block_frames: usize, mut process: impl FnMut(&mut [f32])) {
    let mut offset = 0;
    while offset < samples.len() {
        let end = (offset + block_frames).min(samples.len());
        process(&mut samples[offset..end]);
        offset = end;
    }
}

fn settled_tone(samples: &[f32], freq: f32) -> f32 {
    let skip = (SR as usize) / 10;
    let slice = if samples.len() > skip {
        &samples[skip..]
    } else {
        samples
    };
    tone_level_dbfs(slice, SR, freq)
}

#[test]
fn t04_u_01_eq_1k_peak_plus6() {
    let mut samples = sine(1_000.0, -12.0, 400);
    let input = settled_tone(&samples, 1_000.0);
    let mut eq = peak_band_1k_plus6();
    process_blocked(&mut samples, BLOCK, |block| eq.process(block, 1));
    let output = settled_tone(&samples, 1_000.0);
    let delta = output - input;
    assert!(
        (delta - 6.0).abs() <= 0.5,
        "1 kHz boost {delta} dB (in {input}, out {output})"
    );
}

#[test]
fn t04_u_02_eq_boost_does_not_move_out_of_band() {
    let mut eq = peak_band_1k_plus6();
    for freq in [100.0_f32, 8_000.0] {
        let mut samples = sine(freq, -12.0, 400);
        let input = settled_tone(&samples, freq);
        eq.reset();
        process_blocked(&mut samples, BLOCK, |block| eq.process(block, 1));
        let output = settled_tone(&samples, freq);
        let delta = output - input;
        assert!(
            delta.abs() <= 0.5,
            "{freq} Hz moved {delta} dB (in {input}, out {output})"
        );
    }
}

#[test]
fn t04_u_03_eq_flat_is_unity_gain() {
    let mut eq = Equalizer::new(SR, 1);
    eq.set_smoothing(false);
    for i in 0..EQ_BANDS {
        let mut band = eq.band(i);
        band.gain_db = 0.0;
        eq.set_band(i, band);
    }
    eq.reset();
    let mut samples = sine(1_000.0, -6.0, 400);
    let input = settled_tone(&samples, 1_000.0);
    process_blocked(&mut samples, BLOCK, |block| eq.process(block, 1));
    let output = settled_tone(&samples, 1_000.0);
    assert!(
        (output - input).abs() <= 0.1,
        "flat EQ changed level by {} dB",
        output - input
    );
}

fn gate_sine_burst(threshold_db: f32, attack_ms: f32, hold_ms: f32, release_ms: f32) -> NoiseGate {
    let mut gate = NoiseGate::new(SR, 1);
    gate.set_threshold_db(threshold_db);
    gate.set_attack_ms(attack_ms);
    gate.set_hold_ms(hold_ms);
    gate.set_release_ms(release_ms);
    gate
}

#[test]
fn t04_u_04_gate_opens_and_closes_with_timing() {
    let attack_ms = 20.0_f32;
    let hold_ms = 30.0_f32;
    let release_ms = 80.0_f32;
    let mut gate = gate_sine_burst(-40.0, attack_ms, hold_ms, release_ms);
    let quiet = sine(1_000.0, -60.0, 200);
    let loud = sine(1_000.0, -20.0, 250);
    let mut samples = quiet.clone();
    samples.extend_from_slice(&loud);
    samples.extend_from_slice(&quiet);
    process_blocked(&mut samples, BLOCK, |block| gate.process(block, 1));

    let sr = SR as usize;
    let first = &samples[..sr / 10];
    let open = &samples[sr / 5 + sr / 20..sr / 5 + sr / 8];
    let closed = &samples[samples.len() - sr / 10..];
    assert!(
        peak_dbfs(first) < -55.0,
        "first segment should stay closed, peak {}",
        peak_dbfs(first)
    );
    assert!(
        peak_dbfs(open) > -26.0,
        "middle segment should open, peak {}",
        peak_dbfs(open)
    );
    assert!(
        peak_dbfs(closed) < -55.0,
        "after hold+release should close, peak {}",
        peak_dbfs(closed)
    );
    assert_eq!(gate.state(), GateState::Closed);

    let mut timing_gate = gate_sine_burst(-40.0, attack_ms, 0.0, release_ms);
    let mut attack_signal = sine(1_000.0, -80.0, 50);
    attack_signal.extend_from_slice(&sine(1_000.0, -12.0, 200));
    process_blocked(&mut attack_signal, BLOCK, |block| {
        timing_gate.process(block, 1)
    });
    let measured_attack =
        envelope_ms(&attack_signal, SR, -50.0, -14.0).expect("gate attack envelope");
    assert!(
        (measured_attack - attack_ms).abs() <= attack_ms * 0.20 + 5.0,
        "attack {measured_attack} ms vs {attack_ms} ms"
    );

    let mut release_gate = gate_sine_burst(-10.0, 1.0, 0.0, release_ms);
    let mut release_signal = sine(1_000.0, 0.0, 80);
    release_signal.extend_from_slice(&sine(1_000.0, -80.0, 300));
    // Force open then close: threshold -10 with 0 dBFS opens; -80 closes.
    process_blocked(&mut release_signal, BLOCK, |block| {
        release_gate.process(block, 1)
    });
    let measured_release =
        envelope_ms(&release_signal, SR, -3.0, -40.0).expect("gate release envelope");
    assert!(
        (measured_release - release_ms).abs() <= release_ms * 0.20 + 8.0,
        "release {measured_release} ms vs {release_ms} ms"
    );
}

#[test]
fn t04_u_05_gate_closed_attenuates_at_least_60db() {
    let mut gate = gate_sine_burst(0.0, 1.0, 0.0, 10.0);
    let mut samples = sine(1_000.0, -12.0, 300);
    process_blocked(&mut samples, BLOCK, |block| gate.process(block, 1));
    let skip = SR as usize / 10;
    let residual = peak_dbfs(&samples[skip..]);
    assert!(
        residual <= -72.0,
        "closed gate residual {residual} dBFS (want ≤ -72 = input-60)"
    );
}

#[test]
fn t04_u_06_compressor_steady_ratio() {
    let mut comp = Compressor::new(SR, 1);
    comp.set_threshold_db(-20.0);
    comp.set_ratio(4.0);
    comp.set_attack_ms(0.1);
    comp.set_release_ms(50.0);
    comp.set_makeup_db(0.0);
    let mut samples = sine(1_000.0, -10.0, 500);
    process_blocked(&mut samples, BLOCK, |block| comp.process(block, 1));
    let skip = (SR as usize) / 5;
    let level = tone_level_dbfs(&samples[skip..], SR, 1_000.0);
    assert!(
        (level + 17.5).abs() <= 1.0,
        "compressor output {level} dBFS, expected ≈ -17.5"
    );
}

#[test]
fn t04_u_07_compressor_attack_release_times() {
    let attack_ms = 40.0_f32;
    let release_ms = 80.0_f32;
    let mut comp = Compressor::new(SR, 1);
    comp.set_threshold_db(-24.0);
    comp.set_ratio(20.0);
    comp.set_attack_ms(attack_ms);
    comp.set_release_ms(release_ms);
    comp.set_makeup_db(0.0);
    let mut samples = sine(1_000.0, -40.0, 80);
    samples.extend_from_slice(&sine(1_000.0, -3.0, 250));
    samples.extend_from_slice(&sine(1_000.0, -40.0, 300));
    process_blocked(&mut samples, BLOCK, |block| comp.process(block, 1));
    let attack_region = &samples[(SR as usize * 80 / 1_000)..(SR as usize * 330 / 1_000)];
    let measured_attack = envelope_ms(attack_region, SR, -4.0, -18.0).expect("comp attack");
    assert!(
        (measured_attack - attack_ms).abs() <= attack_ms * 0.20 + 8.0,
        "comp attack {measured_attack} vs {attack_ms}"
    );
    let quiet_in = db_to_linear(-40.0);
    let release_start = SR as usize * 330 / 1_000;
    let mut start_at = None;
    let mut end_at = None;
    for (index, sample) in samples[release_start..].iter().enumerate() {
        let gain_db = 20.0 * (sample.abs() / quiet_in.max(1.0e-12)).max(1.0e-12).log10();
        // Recovering from ~-20 dB GR toward 0 dB.
        if start_at.is_none() && gain_db > -16.0 {
            start_at = Some(index);
        }
        if start_at.is_some() && end_at.is_none() && gain_db > -4.0 {
            end_at = Some(index);
            break;
        }
    }
    let measured_release = (end_at.unwrap() - start_at.unwrap()) as f32 * 1_000.0 / SR as f32;
    assert!(
        (measured_release - release_ms).abs() <= release_ms * 0.20 + 12.0,
        "comp release {measured_release} vs {release_ms}"
    );
}

#[test]
fn t04_u_08_noise_suppressor_improves_snr() {
    let spec = SignalSpec {
        kind: SignalKind::Sine,
        frequency: 1_000.0,
        amplitude_db: -20.0,
        duration_ms: 1_200,
        sample_rate: SR,
        channels: 1,
        seed: 3,
        click_interval_ms: 100,
    };
    let sine_samples = generate(&spec);
    let mut noise_spec = spec.clone();
    noise_spec.kind = SignalKind::Noise;
    noise_spec.amplitude_db = -40.0;
    let noise = generate(&noise_spec);
    let mixed: Vec<f32> = sine_samples
        .iter()
        .zip(noise.iter())
        .map(|(s, n)| s + n)
        .collect();
    let input_snr = snr_db(&mixed, 1_000.0, SR);
    let mut nr = NoiseSuppressor::new(SR, 1);
    nr.set_enabled(true);
    nr.set_strength(100.0);
    nr.set_smoothing(false);
    let mut output = mixed.clone();
    process_blocked(&mut output, BLOCK, |block| nr.process(block, 1));
    let skip = FRAME_SKIP;
    let output_snr = snr_db(&output[skip..], 1_000.0, SR);
    let improvement = output_snr - input_snr;
    assert!(
        improvement >= 10.0,
        "SNR improvement {improvement} dB (in {input_snr}, out {output_snr}); threshold is design T04-U-08 +10 dB"
    );
    assert_eq!(nr.latency_frames(), 480);
    nr.set_enabled(false);
    assert_eq!(nr.latency_frames(), 0);
}

const FRAME_SKIP: usize = 480 * 4;

#[test]
fn t04_u_09_gain_smoothing_reduces_clicks() {
    let mut dry = sine(80.0, -18.0, 250);
    let mut wet = dry.clone();
    let mut eq_click = Equalizer::new(SR, 1);
    eq_click.set_smoothing(false);
    let mut eq_smooth = Equalizer::new(SR, 1);
    eq_smooth.set_smoothing(true);
    let mut extreme = EqBandParams {
        freq_hz: 100.0,
        gain_db: 15.0,
        q: 0.7,
    };
    let stride = 240usize;
    let mut offset = 0;
    while offset < dry.len() {
        extreme.gain_db = if extreme.gain_db > 0.0 { -15.0 } else { 15.0 };
        eq_click.set_band(0, extreme);
        eq_smooth.set_band(0, extreme);
        let end = (offset + stride).min(dry.len());
        eq_click.process(&mut dry[offset..end], 1);
        eq_smooth.process(&mut wet[offset..end], 1);
        offset = end;
    }
    let click_step = max_sample_step(&dry);
    let smooth_step = max_sample_step(&wet);
    assert!(
        smooth_step < click_step,
        "smoothed step {smooth_step} was not smaller than unsmoothed {click_step}"
    );
    assert!(
        smooth_step < 0.05,
        "smoothed max_sample_step {smooth_step} exceeded click threshold 0.05"
    );

    let mut comp_click = Compressor::new(SR, 1);
    comp_click.set_smoothing(false);
    let mut comp_smooth = Compressor::new(SR, 1);
    let mut makeup_in = sine(80.0, -18.0, 250);
    let mut makeup_out = makeup_in.clone();
    offset = 0;
    let mut makeup = 0.0f32;
    while offset < makeup_in.len() {
        makeup = if makeup > 0.0 { 0.0 } else { 18.0 };
        comp_click.set_makeup_db(makeup);
        comp_smooth.set_makeup_db(makeup);
        let end = (offset + stride).min(makeup_in.len());
        comp_click.process(&mut makeup_in[offset..end], 1);
        comp_smooth.process(&mut makeup_out[offset..end], 1);
        offset = end;
    }
    assert!(max_sample_step(&makeup_out) < max_sample_step(&makeup_in));
    assert!(max_sample_step(&makeup_out) < 0.05);

    let mut nr_click = NoiseSuppressor::new(SR, 1);
    nr_click.set_smoothing(false);
    let mut nr_smooth = NoiseSuppressor::new(SR, 1);
    let mut nr_in = sine(80.0, -18.0, 250);
    let mut nr_out = nr_in.clone();
    offset = 0;
    let mut strength = 0.0f32;
    while offset < nr_in.len() {
        strength = if strength > 0.0 { 0.0 } else { 100.0 };
        nr_click.set_strength(strength);
        nr_smooth.set_strength(strength);
        let end = (offset + stride).min(nr_in.len());
        nr_click.process(&mut nr_in[offset..end], 1);
        nr_smooth.process(&mut nr_out[offset..end], 1);
        offset = end;
    }
    assert!(max_sample_step(&nr_out) <= max_sample_step(&nr_in) + 1.0e-4);
    assert!(max_sample_step(&nr_out) < 0.05);

    let mut gate_click = NoiseGate::new(SR, 1);
    gate_click.set_smoothing(false);
    gate_click.set_attack_ms(5.0);
    gate_click.set_release_ms(80.0);
    let mut gate_smooth = NoiseGate::new(SR, 1);
    gate_smooth.set_smoothing(true);
    gate_smooth.set_attack_ms(5.0);
    gate_smooth.set_release_ms(80.0);
    let mut gate_in = sine(80.0, -18.0, 250);
    let mut gate_out = gate_in.clone();
    offset = 0;
    let mut threshold = 0.0f32;
    while offset < gate_in.len() {
        threshold = if threshold > -40.0 { -80.0 } else { 0.0 };
        gate_click.set_threshold_db(threshold);
        gate_smooth.set_threshold_db(threshold);
        let end = (offset + stride).min(gate_in.len());
        gate_click.process(&mut gate_in[offset..end], 1);
        gate_smooth.process(&mut gate_out[offset..end], 1);
        offset = end;
    }
    let gate_click_step = max_sample_step(&gate_in);
    let gate_smooth_step = max_sample_step(&gate_out);
    assert!(
        gate_smooth_step < gate_click_step,
        "gate smoothed step {gate_smooth_step} was not smaller than unsmoothed {gate_click_step}"
    );
    assert!(
        gate_smooth_step < 0.05,
        "gate smoothed max_sample_step {gate_smooth_step} exceeded click threshold 0.05"
    );
}

fn block_size_span(levels: &[f32]) -> f32 {
    levels.iter().cloned().fold(f32::MIN, f32::max)
        - levels.iter().cloned().fold(f32::MAX, f32::min)
}

#[test]
fn t04_u_10_block_size_does_not_change_dsp_response() {
    let mut eq_levels = Vec::new();
    let mut gate_levels = Vec::new();
    let mut comp_levels = Vec::new();
    let mut nr_levels = Vec::new();
    for block in [64usize, 480, 4096] {
        let mut eq_samples = sine(1_000.0, -12.0, 400);
        let mut eq = peak_band_1k_plus6();
        process_blocked(&mut eq_samples, block, |chunk| eq.process(chunk, 1));
        eq_levels.push(settled_tone(&eq_samples, 1_000.0));

        let mut gate_samples = sine(1_000.0, -12.0, 400);
        let mut gate = gate_sine_burst(-80.0, 1.0, 0.0, 10.0);
        process_blocked(&mut gate_samples, block, |chunk| gate.process(chunk, 1));
        gate_levels.push(settled_tone(&gate_samples, 1_000.0));

        let mut comp_samples = sine(1_000.0, -10.0, 500);
        let mut comp = Compressor::new(SR, 1);
        comp.set_threshold_db(-20.0);
        comp.set_ratio(4.0);
        comp.set_attack_ms(0.1);
        comp.set_release_ms(50.0);
        comp.set_makeup_db(0.0);
        process_blocked(&mut comp_samples, block, |chunk| comp.process(chunk, 1));
        let skip = (SR as usize) / 5;
        comp_levels.push(tone_level_dbfs(&comp_samples[skip..], SR, 1_000.0));

        let mut nr_samples = sine(1_000.0, -20.0, 800);
        let mut nr = NoiseSuppressor::new(SR, 1);
        nr.set_enabled(true);
        nr.set_strength(100.0);
        nr.set_smoothing(false);
        process_blocked(&mut nr_samples, block, |chunk| nr.process(chunk, 1));
        nr_levels.push(tone_level_dbfs(&nr_samples[FRAME_SKIP..], SR, 1_000.0));
    }
    assert!(
        block_size_span(&eq_levels) <= 0.5,
        "EQ level depended on block size: {eq_levels:?}"
    );
    assert!(
        block_size_span(&gate_levels) <= 0.5,
        "gate level depended on block size: {gate_levels:?}"
    );
    assert!(
        block_size_span(&comp_levels) <= 1.0,
        "compressor level depended on block size: {comp_levels:?}"
    );
    assert!(
        block_size_span(&nr_levels) <= 1.5,
        "noise suppressor level depended on block size: {nr_levels:?}"
    );
}

#[test]
fn t04_u_11_reset_replays_identical_output() {
    let input = sine(440.0, -9.0, 120);
    let mut eq = peak_band_1k_plus6();
    let mut a = input.clone();
    eq.process(&mut a, 1);
    eq.reset();
    let mut b = input.clone();
    eq.process(&mut b, 1);
    assert_eq!(a, b, "EQ reset leaked state");

    let mut gate = gate_sine_burst(-40.0, 5.0, 10.0, 40.0);
    let mut ga = input.clone();
    gate.process(&mut ga, 1);
    gate.reset();
    let mut gb = input.clone();
    gate.process(&mut gb, 1);
    assert_eq!(ga, gb, "gate reset leaked state");

    let mut comp = Compressor::new(SR, 1);
    let mut ca = input.clone();
    comp.process(&mut ca, 1);
    comp.reset();
    let mut cb = input.clone();
    comp.process(&mut cb, 1);
    assert_eq!(ca, cb, "compressor reset leaked state");

    let mut nr = NoiseSuppressor::new(SR, 1);
    let mut na = input.clone();
    nr.process(&mut na, 1);
    nr.reset();
    let mut nb = input.clone();
    nr.process(&mut nb, 1);
    assert_eq!(na, nb, "noise suppressor reset leaked state");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn t04_p_01_dsp_stays_finite(
        seed in 1u64..10_000,
        eq_gain in -15.0f32..15.0,
        threshold in -80.0f32..0.0,
        ratio in 1.0f32..20.0,
        makeup in 0.0f32..24.0,
        strength in 0.0f32..100.0,
        amp_db in -24.0f32..-6.0,
    ) {
        let spec = SignalSpec {
            kind: SignalKind::Noise,
            amplitude_db: amp_db,
            duration_ms: 80,
            sample_rate: SR,
            channels: 1,
            seed,
            frequency: 1_000.0,
            click_interval_ms: 100,
        };
        let input = generate(&spec);
        let mut eq = Equalizer::new(SR, 1);
        let mut band = eq.band(2);
        band.gain_db = eq_gain;
        eq.set_band(2, band);
        let mut eq_out = input.clone();
        eq.process(&mut eq_out, 1);

        let mut gate = NoiseGate::new(SR, 1);
        gate.set_threshold_db(threshold);
        let mut gate_out = input.clone();
        gate.process(&mut gate_out, 1);

        let mut comp = Compressor::new(SR, 1);
        comp.set_ratio(ratio);
        comp.set_makeup_db(makeup);
        let mut comp_out = input.clone();
        comp.process(&mut comp_out, 1);

        let mut nr = NoiseSuppressor::new(SR, 1);
        nr.set_strength(strength);
        let mut nr_out = input.clone();
        nr.process(&mut nr_out, 1);

        for (name, out) in [
            ("eq", &eq_out),
            ("gate", &gate_out),
            ("comp", &comp_out),
            ("nr", &nr_out),
        ] {
            prop_assert!(out.iter().all(|s| s.is_finite()), "{name} produced non-finite");
            let peak = peak_dbfs(out);
            prop_assert!(
                peak < 40.0,
                "{name} diverged to {peak} dBFS"
            );
        }
    }
}
