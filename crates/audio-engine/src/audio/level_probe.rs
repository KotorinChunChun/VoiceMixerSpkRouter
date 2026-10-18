//! 明瞭な音の入出力時刻を照合するための任意観測口。PCMを保持せず、音声を待たせない。
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;

use super::clock::frames_to_100ns;

#[derive(Debug, Clone, Copy)]
pub struct LevelObservation {
    pub at_100ns: u64,
    pub duration_100ns: u64,
    pub rms: f32,
}

/// 音声ワーカーが書き、観測者1つが読む。無効時は音量計算も行わない。
pub struct LevelProbe {
    enabled: AtomicBool,
    epoch: AtomicU64,
    lost: AtomicU64,
    samples: ArrayQueue<LevelObservation>,
}
impl Default for LevelProbe {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            samples: ArrayQueue::new(1024),
        }
    }
}
impl LevelProbe {
    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::AcqRel) != enabled {
            while self.samples.pop().is_some() {}
        }
    }
    pub fn restart(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn revision(&self) -> (u64, u64) {
        (
            self.epoch.load(Ordering::Acquire),
            self.lost.load(Ordering::Acquire),
        )
    }
    pub fn pop(&self) -> Option<LevelObservation> {
        self.samples.pop()
    }
    pub fn observe(&self, samples: &[f32], at_100ns: u64, sample_rate: u32, channels: usize) {
        if !self.enabled.load(Ordering::Acquire) || sample_rate == 0 || channels == 0 {
            return;
        }
        let frames_per_bin = (sample_rate as usize / 100).max(1);
        let mut offset = 0;
        for chunk in samples.chunks(frames_per_bin * channels) {
            if chunk.is_empty() || !chunk.len().is_multiple_of(channels) {
                continue;
            }
            let frames = chunk.len() / channels;
            let rms = (chunk.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>()
                / chunk.len() as f64)
                .sqrt() as f32;
            let observation = LevelObservation {
                at_100ns: at_100ns.saturating_add(frames_to_100ns(offset, sample_rate)),
                duration_100ns: frames_to_100ns(frames, sample_rate),
                rms,
            };
            if self.samples.push(observation).is_err() {
                self.lost.fetch_add(1, Ordering::Relaxed);
            }
            offset += frames;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_is_optional_bounded_and_keeps_sample_times() {
        let probe = LevelProbe::default();
        let audio = vec![0.25; 480 * 2 * 3];
        probe.observe(&audio, 1_000_000, 48_000, 2);
        assert!(probe.pop().is_none());
        probe.set_enabled(true);
        probe.observe(&audio, 1_000_000, 48_000, 2);
        for index in 0..3 {
            let sample = probe.pop().unwrap();
            assert_eq!(sample.at_100ns, 1_000_000 + index * 100_000);
            assert_eq!(sample.duration_100ns, 100_000);
            assert_eq!(sample.rms, 0.25);
        }
        for _ in 0..1100 {
            probe.observe(&audio[..960], 1_000_000, 48_000, 2);
        }
        assert_eq!(probe.revision().1, 76);
        probe.restart();
        assert_eq!(probe.revision().0, 1);
        probe.set_enabled(false);
        assert!(probe.pop().is_none());
        assert_eq!(audio[0], 0.25);
    }
}
