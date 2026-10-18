//! Packet境界をまたいで位相を保持する軽量linear resampler。
use crate::error::{BackendError, Result};

pub struct StreamResampler {
    channels: usize,
    samples: Vec<f32>,
    phase: f64,
}

impl StreamResampler {
    pub fn new(channels: u16) -> Result<Self> {
        if channels == 0 {
            return Err(BackendError::InvalidOperation(
                "resampler channels must be positive".into(),
            ));
        }
        Ok(Self {
            channels: channels as usize,
            samples: Vec::new(),
            phase: 0.0,
        })
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.phase = 0.0;
    }

    pub fn process(&mut self, input: &[f32], output_per_input: f64) -> Result<Vec<f32>> {
        if !output_per_input.is_finite() || !(0.125..=8.0).contains(&output_per_input) {
            return Err(BackendError::InvalidOperation(
                "resampler ratio must be within 0.125..=8".into(),
            ));
        }
        if input.len() % self.channels != 0 {
            return Err(BackendError::InvalidOperation(
                "resampler input is not frame aligned".into(),
            ));
        }
        if (output_per_input - 1.0).abs() < f64::EPSILON
            && self.samples.is_empty()
            && self.phase == 0.0
        {
            return Ok(input.to_vec());
        }
        self.samples.extend_from_slice(input);
        let frames = self.samples.len() / self.channels;
        let mut output = Vec::new();
        while self.phase + 1.0 < frames as f64 {
            let lower = self.phase.floor() as usize;
            let fraction = (self.phase - lower as f64) as f32;
            for channel in 0..self.channels {
                let a = self.samples[lower * self.channels + channel];
                let b = self.samples[(lower + 1) * self.channels + channel];
                output.push(a + (b - a) * fraction);
            }
            self.phase += 1.0 / output_per_input;
        }
        // Keep one source frame for interpolation. With large downsampling the
        // phase can advance beyond this packet; retain that excess for later
        // packets instead of draining past the buffered samples.
        let consumed = (self.phase.floor() as usize).min(frames.saturating_sub(1));
        if consumed != 0 {
            self.samples.drain(..consumed * self.channels);
            self.phase -= consumed as f64;
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect()
    }
    #[test]
    fn ratio_one_is_bitwise_pcm() {
        let mut r = StreamResampler::new(2).unwrap();
        let x = ramp(128);
        assert_eq!(r.process(&x, 1.0).unwrap(), x);
    }
    #[test]
    fn packet_partition_keeps_continuous_samples() {
        let x = ramp(1000);
        let mut whole = StreamResampler::new(2).unwrap();
        let expected = whole.process(&x, 48000.0 / 44100.0).unwrap();
        let mut split = StreamResampler::new(2).unwrap();
        let mut actual = Vec::new();
        for p in x.chunks(2 * 127) {
            actual.extend(split.process(p, 48000.0 / 44100.0).unwrap())
        }
        assert_eq!(actual, expected);
    }
    #[test]
    fn ppm_frame_count_accumulates() {
        for ratio in [0.999, 1.001] {
            let mut r = StreamResampler::new(2).unwrap();
            let mut count = 0;
            for _ in 0..1000 {
                count += r.process(&ramp(128), ratio).unwrap().len() / 2;
            }
            assert!((count as f64 - 128000.0 * ratio).abs() <= 1.0);
        }
    }
    #[test]
    fn reset_discards_previous_packet() {
        let mut r = StreamResampler::new(2).unwrap();
        let _ = r.process(&vec![1.0; 20], 1.1).unwrap();
        r.reset();
        let y = r.process(&vec![2.0; 20], 1.1).unwrap();
        assert!(y.iter().all(|v| *v == 2.0));
    }

    #[test]
    fn downsampled_packets_keep_phase_without_overdraining() {
        let input = ramp(1024);
        let mut whole = StreamResampler::new(2).unwrap();
        let expected = whole.process(&input, 0.25).unwrap();
        for size in [127, 1] {
            let mut split = StreamResampler::new(2).unwrap();
            let mut actual = Vec::new();
            for packet in input.chunks(size * 2) {
                actual.extend(split.process(packet, 0.25).unwrap());
            }
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn rejects_unbounded_ratio() {
        let mut resampler = StreamResampler::new(2).unwrap();
        assert!(resampler.process(&[0.0, 0.0], 8.1).is_err());
        assert!(resampler.process(&[0.0, 0.0], 0.124).is_err());
    }
}
