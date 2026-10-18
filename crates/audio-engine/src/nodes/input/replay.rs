use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc,
};

use crate::audio::format::INTERNAL_SAMPLE_RATE;

/// Lock-free circular replay tap. Each sample slot is an atomic f32 bit pattern.
#[derive(Clone)]
pub struct ReplayTap {
    slots: Arc<Vec<AtomicU32>>,
    write_index: Arc<AtomicU64>,
}

impl ReplayTap {
    pub fn new(seconds: usize) -> Self {
        let samples = seconds.max(1) * INTERNAL_SAMPLE_RATE as usize * 2;
        Self {
            slots: Arc::new((0..samples).map(|_| AtomicU32::new(0)).collect()),
            write_index: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn push_interleaved(&self, samples: &[f32]) {
        let start = self
            .write_index
            .fetch_add(samples.len() as u64, Ordering::AcqRel);
        let len = self.slots.len() as u64;
        for (offset, sample) in samples.iter().enumerate() {
            let idx = ((start + offset as u64) % len) as usize;
            self.slots[idx].store(sample.to_bits(), Ordering::Release);
        }
    }

    pub fn snapshot_last_seconds(&self, seconds: f32) -> Vec<f32> {
        let wanted =
            ((seconds.max(0.0) * INTERNAL_SAMPLE_RATE as f32 * 2.0) as usize).min(self.slots.len());
        let end = self.write_index.load(Ordering::Acquire);
        let available = (end as usize).min(self.slots.len());
        let count = wanted.min(available);
        let start = end.saturating_sub(count as u64);
        let len = self.slots.len() as u64;
        (0..count)
            .map(|i| {
                let idx = ((start + i as u64) % len) as usize;
                f32::from_bits(self.slots[idx].load(Ordering::Acquire))
            })
            .collect()
    }
}

use crate::audio::{buffer::AudioRingBuffer, engine::AudioSource, format::AudioBlock};
use crate::error::Result;

/// One-shot replay playback. Triggering replaces/reloads the ring from a tap
/// snapshot, while the graph thread only performs non-blocking ring reads.
#[derive(Clone)]
pub struct ReplayPlaybackSource {
    ring: AudioRingBuffer,
}

impl ReplayPlaybackSource {
    pub fn from_tap(tap: &ReplayTap, seconds: f32) -> Self {
        let snapshot = tap.snapshot_last_seconds(seconds);
        let ring = AudioRingBuffer::new(snapshot.len().max(960));
        let _ = ring.push_slice(&snapshot);
        Self { ring }
    }

    pub fn stop(&self) {
        let mut scratch = vec![0.0; 8_192];
        while self.ring.len_samples() > 0 {
            self.ring.pop_into(&mut scratch);
        }
    }
}

impl AudioSource for ReplayPlaybackSource {
    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        block.clear();
        let _ = self.ring.pop_into(&mut block.samples);
        Ok(())
    }
}
