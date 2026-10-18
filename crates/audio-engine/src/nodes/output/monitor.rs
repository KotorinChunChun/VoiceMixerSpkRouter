use crate::audio::{buffer::AudioRingBuffer, engine::AudioSink, format::AudioBlock};
use crate::error::Result;

#[derive(Clone)]
pub struct RingOutputSink {
    pub ring: AudioRingBuffer,
}
impl RingOutputSink {
    pub fn new(ring: AudioRingBuffer) -> Self {
        Self { ring }
    }
}
impl AudioSink for RingOutputSink {
    fn pause(&self) {
        self.ring.clear();
    }
    fn write(&self, block: &AudioBlock) -> Result<()> {
        let _ = self.ring.push_slice(&block.samples);
        Ok(())
    }
}
