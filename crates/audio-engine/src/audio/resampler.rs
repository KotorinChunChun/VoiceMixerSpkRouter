use super::format::{AudioBlock, AudioFormat, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE};

/// Lightweight linear resampler used for drift correction and adapter fallback.
/// Device adapters should prefer OS/autoconvert or a higher quality resampler
/// for large fixed-rate conversions.
pub fn resample_interleaved_linear(
    input: &[f32],
    input_rate: u32,
    input_channels: u16,
    output_rate: u32,
    output_channels: u16,
    ratio_correction: f32,
) -> AudioBlock {
    if input.is_empty() || input_channels == 0 || output_channels == 0 {
        return AudioBlock::silence(0);
    }
    let in_frames = input.len() / input_channels as usize;
    if in_frames == 0 {
        return AudioBlock::silence(0);
    }
    let effective_ratio = (output_rate as f64 / input_rate as f64) * ratio_correction as f64;
    let out_frames = ((in_frames as f64) * effective_ratio).max(1.0).round() as usize;
    let mut out = vec![0.0f32; out_frames * output_channels as usize];

    for out_frame in 0..out_frames {
        let src = (out_frame as f64 / effective_ratio).min((in_frames - 1) as f64);
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(in_frames - 1);
        let frac = (src - i0 as f64) as f32;
        for out_ch in 0..output_channels as usize {
            let src_ch = if input_channels == 1 {
                0
            } else {
                out_ch.min(input_channels as usize - 1)
            };
            let a = input[i0 * input_channels as usize + src_ch];
            let b = input[i1 * input_channels as usize + src_ch];
            out[out_frame * output_channels as usize + out_ch] = a + (b - a) * frac;
        }
    }

    AudioBlock {
        format: AudioFormat {
            sample_rate: output_rate,
            channels: output_channels,
        },
        samples: out,
        capture_time_100ns: None,
        accuracy: crate::audio::clock::LatencyAccuracy::HostEstimated,
    }
}

pub fn to_internal(input: &[f32], rate: u32, channels: u16) -> AudioBlock {
    resample_interleaved_linear(
        input,
        rate,
        channels,
        INTERNAL_SAMPLE_RATE,
        INTERNAL_CHANNELS,
        1.0,
    )
}
