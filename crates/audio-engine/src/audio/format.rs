use serde::{Deserialize, Serialize};

use crate::audio::clock::{frames_to_100ns, oldest_capture_time, LatencyAccuracy};

pub const INTERNAL_SAMPLE_RATE: u32 = 48_000;
pub const INTERNAL_CHANNELS: u16 = 2;
pub const DEFAULT_BLOCK_FRAMES: usize = 480; // 10 ms @ 48 kHz

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub const INTERNAL: Self = Self {
        sample_rate: INTERNAL_SAMPLE_RATE,
        channels: INTERNAL_CHANNELS,
    };
}

#[derive(Debug, Clone)]
pub struct AudioBlock {
    pub format: AudioFormat,
    pub samples: Vec<f32>,
    pub capture_time_100ns: Option<u64>,
    pub accuracy: LatencyAccuracy,
}

impl AudioBlock {
    pub fn silence(frames: usize) -> Self {
        Self {
            format: AudioFormat::INTERNAL,
            samples: vec![0.0; frames * INTERNAL_CHANNELS as usize],
            capture_time_100ns: None,
            accuracy: LatencyAccuracy::HostEstimated,
        }
    }

    pub fn from_interleaved_stereo(samples: Vec<f32>) -> Self {
        debug_assert_eq!(samples.len() % 2, 0);
        Self {
            format: AudioFormat::INTERNAL,
            samples,
            capture_time_100ns: None,
            accuracy: LatencyAccuracy::HostEstimated,
        }
    }

    pub fn from_mono(mono: &[f32]) -> Self {
        let mut samples = Vec::with_capacity(mono.len() * 2);
        for &sample in mono {
            samples.push(sample);
            samples.push(sample);
        }
        Self::from_interleaved_stereo(samples)
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / self.format.channels as usize
    }

    pub fn clear(&mut self) {
        self.samples.fill(0.0);
        self.capture_time_100ns = None;
        self.accuracy = LatencyAccuracy::HostEstimated;
    }

    pub fn copy_latency_from(&mut self, other: &Self) {
        self.capture_time_100ns = other.capture_time_100ns;
        self.accuracy = other.accuracy;
    }

    pub fn merge_oldest_latency(&mut self, other: &Self) {
        match (self.capture_time_100ns, other.capture_time_100ns) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(other_time)) => {
                self.capture_time_100ns = Some(other_time);
                self.accuracy = other.accuracy;
            }
            (Some(self_time), Some(other_time)) => {
                self.accuracy = self.accuracy.merge(other.accuracy);
                if other_time < self_time {
                    self.capture_time_100ns = Some(other_time);
                }
            }
        }
    }

    pub fn set_capture_stamp(&mut self, time_100ns: u64, accuracy: LatencyAccuracy) {
        self.capture_time_100ns = Some(time_100ns);
        self.accuracy = accuracy;
    }

    pub fn stamp_host_now(&mut self) {
        self.set_capture_stamp(
            crate::audio::clock::now_100ns(),
            LatencyAccuracy::HostEstimated,
        );
    }

    pub fn shift_capture_time(&mut self, frames: usize) {
        if let Some(time) = self.capture_time_100ns {
            self.capture_time_100ns =
                Some(time.saturating_sub(frames_to_100ns(frames, self.format.sample_rate)));
        }
    }

    pub fn apply_gain_db(&mut self, gain_db: f32) {
        let gain = db_to_linear(gain_db);
        for sample in &mut self.samples {
            *sample *= gain;
        }
    }

    pub fn add_scaled(&mut self, other: &AudioBlock, gain_db: f32) {
        let gain = db_to_linear(gain_db);
        let len = self.samples.len().min(other.samples.len());
        for i in 0..len {
            self.samples[i] += other.samples[i] * gain;
        }
        self.merge_oldest_latency(other);
    }

    pub fn clipped(&self) -> bool {
        self.samples.iter().any(|x| x.abs() > 1.0)
    }
}

pub fn oldest_block_capture_time(times: &[Option<u64>]) -> Option<u64> {
    oldest_capture_time(times)
}

#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}
