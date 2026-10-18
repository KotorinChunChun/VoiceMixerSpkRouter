use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::audio::clock::{frames_to_100ns, CaptureClock, LatencyAccuracy};
use crate::audio::{buffer::AudioRingBuffer, engine::AudioSource, format::AudioBlock};
use crate::error::Result;

#[derive(Clone)]
pub struct RingInputSource {
    pub ring: AudioRingBuffer,
    clock: Option<Arc<CaptureClock>>,
    reported_discards: Arc<AtomicU64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_microphone_resumes_from_recent_stereo_audio() {
        let ring = AudioRingBuffer::new_live_stereo(9_600);
        let clock = Arc::new(CaptureClock::default());
        clock.observe(30_000_000, LatencyAccuracy::DeviceReported);
        let source = RingInputSource::with_clock(ring.clone(), clock);
        // 消費を2秒止める。古い先頭を出さず、最新40msから再開する。
        let input: Vec<f32> = (0..96_000).flat_map(|n| [n as f32, -(n as f32)]).collect();
        ring.push_slice(&input);
        let mut block = AudioBlock::silence(480);
        source.read_into(&mut block).unwrap();
        assert_eq!(block.samples[0], 94_080.0);
        assert!(ring.len_samples() <= 1_440 * 2);
        assert_eq!(ring.discarded_samples(), 94_080 * 2);
        assert_eq!(block.capture_time_100ns, Some(29_700_000));
        assert_eq!(block.accuracy, LatencyAccuracy::HostEstimated);
        for frame in block.samples.chunks_exact(2) {
            assert_eq!(frame[0], -frame[1]);
        }
    }

    #[test]
    fn normal_fifo_retains_file_audio_and_empty_input_does_not_claim_capture_time() {
        let ring = AudioRingBuffer::new(20_000);
        let input: Vec<f32> = (0..8_000).flat_map(|n| [n as f32, -(n as f32)]).collect();
        ring.push_slice(&input);
        let source = RingInputSource::new(ring.clone());
        let mut block = AudioBlock::silence(480);
        source.read_into(&mut block).unwrap();
        assert_eq!(block.samples[0], 0.0);
        assert_eq!(ring.len_samples(), (8_000 - 480) * 2);
        let clock = Arc::new(CaptureClock::default());
        clock.observe(30_000_000, LatencyAccuracy::DeviceReported);
        let empty = RingInputSource::with_clock(AudioRingBuffer::new_live_stereo(960), clock);
        empty.read_into(&mut block).unwrap();
        assert_eq!(block.capture_time_100ns, None);
        assert!(block.samples.iter().all(|sample| *sample == 0.0));
    }
}
impl RingInputSource {
    pub fn new(ring: AudioRingBuffer) -> Self {
        Self {
            ring,
            clock: None,
            reported_discards: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_clock(ring: AudioRingBuffer, clock: Arc<CaptureClock>) -> Self {
        Self {
            ring,
            clock: Some(clock),
            reported_discards: Arc::new(AtomicU64::new(0)),
        }
    }
}
impl AudioSource for RingInputSource {
    fn take_discarded_samples(&self) -> u64 {
        let total = self.ring.discarded_samples();
        total.saturating_sub(self.reported_discards.swap(total, Ordering::Relaxed))
    }
    fn read_into(&self, block: &mut AudioBlock) -> Result<()> {
        block.clear();
        if self.ring.is_live() {
            // 4ブロック、20〜200ms。ブロックサイズ変更にも読み出し時に追従する。
            self.ring
                .trim_live_frames((block.frames() * 4).clamp(960, 9_600));
        }
        let queued_frames = self.ring.len_samples() / 2;
        let read = self.ring.pop_into(&mut block.samples);
        if let Some(clock) = &self.clock {
            if read > 0 {
                if let Some((time, accuracy)) = clock.stamp() {
                    // 最新パケットの先頭時刻しか共有されていないため保守的な推定。
                    // 古いPCMへ最新時刻をそのまま付け、滞留を隠してはいけない。
                    let backlog = queued_frames.saturating_sub(block.frames());
                    block.set_capture_stamp(
                        time.saturating_sub(frames_to_100ns(backlog, 48_000)),
                        if backlog > 0 {
                            LatencyAccuracy::HostEstimated
                        } else {
                            accuracy
                        },
                    );
                }
            }
        }
        Ok(())
    }
}
