use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct MeterFrame {
    pub peak: f32,
    pub rms: f32,
    pub clipped: bool,
}

pub fn measure(samples: &[f32]) -> MeterFrame {
    if samples.is_empty() {
        return MeterFrame::default();
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    let mut clipped = false;
    for &sample in samples {
        let abs = sample.abs();
        peak = peak.max(abs);
        sum_sq += (sample as f64) * (sample as f64);
        clipped |= abs > 1.0;
    }
    MeterFrame {
        peak,
        rms: (sum_sq / samples.len() as f64).sqrt() as f32,
        clipped,
    }
}

/// Lock-free meter snapshot used between realtime and UI threads.
#[derive(Debug, Default)]
pub struct AtomicMeter {
    peak_bits: AtomicU32,
    rms_bits: AtomicU32,
    clipped: AtomicU32,
}

impl AtomicMeter {
    pub fn store(&self, frame: MeterFrame) {
        self.peak_bits
            .store(frame.peak.to_bits(), Ordering::Relaxed);
        self.rms_bits.store(frame.rms.to_bits(), Ordering::Relaxed);
        self.clipped.store(frame.clipped as u32, Ordering::Relaxed);
    }

    pub fn load(&self) -> MeterFrame {
        MeterFrame {
            peak: f32::from_bits(self.peak_bits.load(Ordering::Relaxed)),
            rms: f32::from_bits(self.rms_bits.load(Ordering::Relaxed)),
            clipped: self.clipped.load(Ordering::Relaxed) != 0,
        }
    }
}
