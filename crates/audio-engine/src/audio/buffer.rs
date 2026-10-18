use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use super::format::AudioBlock;

/// Bounded lock-free interleaved f32 queue. Producers never block; overflow is
/// reported to the caller so device callbacks can drop instead of stalling.
#[derive(Clone)]
pub struct AudioRingBuffer {
    queue: Arc<ArrayQueue<f32>>,
    overwrite_oldest: bool,
    live_frames: Option<Arc<ArrayQueue<[f32; 2]>>>,
    discarded_samples: Arc<AtomicU64>,
}

impl AudioRingBuffer {
    pub fn new(sample_capacity: usize) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(sample_capacity.max(2))),
            overwrite_oldest: false,
            live_frames: None,
            discarded_samples: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Creates a realtime monitor queue that always retains the newest audio.
    /// When a consumer falls behind, old samples are discarded instead of
    /// turning the backlog into audible latency.
    pub fn new_overwriting(sample_capacity: usize) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(sample_capacity.max(2))),
            overwrite_oldest: true,
            live_frames: None,
            discarded_samples: Arc::new(AtomicU64::new(0)),
        }
    }

    /// ライブ入力専用。L/Rを不可分のフレームとして扱い、満杯時は最古を捨てる。
    pub fn new_live_stereo(capacity_frames: usize) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(2)),
            overwrite_oldest: false,
            live_frames: Some(Arc::new(ArrayQueue::new(capacity_frames.max(1)))),
            discarded_samples: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn is_live(&self) -> bool {
        self.live_frames.is_some()
    }

    pub fn discarded_samples(&self) -> u64 {
        self.discarded_samples.load(Ordering::Relaxed)
    }

    /// 大きな停止後は消費側でも滞留を制限する。通常のFIFOには適用しない。
    pub fn trim_live_frames(&self, keep_frames: usize) {
        if let Some(queue) = &self.live_frames {
            let excess = queue.len().saturating_sub(keep_frames);
            for _ in 0..excess {
                if queue.pop().is_some() {
                    self.discarded_samples.fetch_add(2, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn push_slice(&self, samples: &[f32]) -> usize {
        if let Some(queue) = &self.live_frames {
            debug_assert_eq!(samples.len() % 2, 0);
            for frame in samples.chunks_exact(2) {
                if queue.force_push([frame[0], frame[1]]).is_some() {
                    self.discarded_samples.fetch_add(2, Ordering::Relaxed);
                }
            }
            return samples.len() / 2 * 2;
        }
        let mut pushed = 0;
        for &sample in samples {
            if let Err(sample) = self.queue.push(sample) {
                if !self.overwrite_oldest {
                    break;
                }
                let _ = self.queue.pop();
                if self.queue.push(sample).is_err() {
                    break;
                }
            }
            pushed += 1;
        }
        pushed
    }

    pub fn pop_into(&self, out: &mut [f32]) -> usize {
        if let Some(queue) = &self.live_frames {
            let mut read = 0;
            for slot in out.chunks_exact_mut(2) {
                let Some(frame) = queue.pop() else { break };
                slot.copy_from_slice(&frame);
                read += 2;
            }
            return read;
        }
        let mut read = 0;
        for slot in out.iter_mut() {
            match self.queue.pop() {
                Some(value) => {
                    *slot = value;
                    read += 1;
                }
                None => break,
            }
        }
        read
    }

    pub fn pop_block_or_silence(&self, frames: usize) -> AudioBlock {
        let mut block = AudioBlock::silence(frames);
        let _ = self.pop_into(&mut block.samples);
        block
    }

    pub fn clear(&self) {
        if let Some(queue) = &self.live_frames {
            while queue.pop().is_some() {}
        }
        while self.queue.pop().is_some() {}
    }

    pub fn len_samples(&self) -> usize {
        self.live_frames
            .as_ref()
            .map_or_else(|| self.queue.len(), |queue| queue.len() * 2)
    }

    pub fn capacity_samples(&self) -> usize {
        self.live_frames
            .as_ref()
            .map_or_else(|| self.queue.capacity(), |queue| queue.capacity() * 2)
    }

    pub fn fill_ratio(&self) -> f32 {
        self.len_samples() as f32 / self.capacity_samples() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::AudioRingBuffer;

    #[test]
    fn overwriting_ring_keeps_the_newest_samples() {
        let ring = AudioRingBuffer::new_overwriting(4);
        assert_eq!(ring.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0]), 5);
        let mut output = [0.0; 4];
        assert_eq!(ring.pop_into(&mut output), 4);
        assert_eq!(output, [2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn regular_ring_preserves_existing_overflow_behavior() {
        let ring = AudioRingBuffer::new(2);
        assert_eq!(ring.push_slice(&[1.0, 2.0, 3.0]), 2);
        let mut output = [0.0; 2];
        assert_eq!(ring.pop_into(&mut output), 2);
        assert_eq!(output, [1.0, 2.0]);
    }

    #[test]
    fn concurrent_live_overflow_never_splits_left_and_right() {
        let ring = AudioRingBuffer::new_live_stereo(32);
        let producer = ring.clone();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_finished = finished.clone();
        let worker = std::thread::spawn(move || {
            for n in 1..100_000 {
                producer.push_slice(&[n as f32, -(n as f32)]);
            }
            worker_finished.store(true, std::sync::atomic::Ordering::Release);
        });
        let mut out = [0.0; 14];
        while !finished.load(std::sync::atomic::Ordering::Acquire) || ring.len_samples() > 0 {
            let read = ring.pop_into(&mut out);
            assert_eq!(read % 2, 0);
            for frame in out[..read].chunks_exact(2) {
                assert_eq!(frame[0], -frame[1]);
            }
        }
        worker.join().unwrap();
    }
}

#[derive(Debug, Clone)]
pub struct JitterPolicy {
    pub target_fill_ratio: f32,
    pub start_threshold_ratio: f32,
}

impl Default for JitterPolicy {
    fn default() -> Self {
        Self {
            target_fill_ratio: 0.5,
            start_threshold_ratio: 0.25,
        }
    }
}

/// Network-facing jitter queue. Network writes and audio reads are decoupled;
/// the audio side receives silence on underflow rather than waiting.
#[derive(Clone)]
pub struct JitterBuffer {
    ring: AudioRingBuffer,
    target_fill_milli: Arc<AtomicU32>,
    start_threshold_milli: Arc<AtomicU32>,
    started: Arc<AtomicBool>,
}

impl JitterBuffer {
    pub fn new(sample_capacity: usize, policy: JitterPolicy) -> Self {
        Self {
            ring: AudioRingBuffer::new(sample_capacity),
            target_fill_milli: Arc::new(AtomicU32::new(ratio_to_milli(policy.target_fill_ratio))),
            start_threshold_milli: Arc::new(AtomicU32::new(ratio_to_milli(
                policy.start_threshold_ratio,
            ))),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_target_fill_ratio(&self, ratio: f32) {
        let clamped = ratio.clamp(0.05, 0.95);
        self.target_fill_milli
            .store(ratio_to_milli(clamped), Ordering::Release);
        self.start_threshold_milli.store(
            ratio_to_milli((clamped * 0.5).clamp(0.05, 0.9)),
            Ordering::Release,
        );
    }

    /// Drops buffered samples and requires a new start threshold after an
    /// input ownership change, so a reconnect never emits stale PCM.
    pub fn reset(&self) {
        self.ring.clear();
        self.started.store(false, Ordering::Release);
    }

    pub fn push(&self, samples: &[f32]) -> usize {
        let written = self.ring.push_slice(samples);
        if self.ring.fill_ratio()
            >= milli_to_ratio(self.start_threshold_milli.load(Ordering::Acquire))
        {
            self.started.store(true, Ordering::Release);
        }
        written
    }

    pub fn read_into(&self, out: &mut [f32]) -> usize {
        out.fill(0.0);
        if !self.started.load(Ordering::Acquire) {
            return 0;
        }
        self.ring.pop_into(out)
    }

    pub fn read_block(&self, frames: usize) -> AudioBlock {
        let mut block = AudioBlock::silence(frames);
        let _ = self.read_into(&mut block.samples);
        block
    }

    pub fn fill_ratio(&self) -> f32 {
        self.ring.fill_ratio()
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub fn target_fill_ratio(&self) -> f32 {
        milli_to_ratio(self.target_fill_milli.load(Ordering::Acquire))
    }
}

fn ratio_to_milli(ratio: f32) -> u32 {
    (ratio.clamp(0.0, 1.0) * 1000.0).round() as u32
}

fn milli_to_ratio(milli: u32) -> f32 {
    milli as f32 / 1000.0
}
