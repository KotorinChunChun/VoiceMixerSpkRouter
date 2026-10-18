use crate::audio::{format::AudioBlock, graph::DelayConfig};

/// User-configurable delay with one maximum-size allocation at construction.
pub struct DelayProcessor {
    buffer: Vec<f32>,
    cursor: usize,
}

impl DelayProcessor {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let max_frames = (DelayConfig::MAX_DELAY_MS as usize * sample_rate as usize) / 1_000;
        Self {
            buffer: vec![0.0; max_frames.saturating_mul(channels as usize).max(1)],
            cursor: 0,
        }
    }

    pub fn process_in_place(&mut self, block: &mut AudioBlock, config: DelayConfig) {
        let delay_frames =
            (config.delay_ms * block.format.sample_rate as f32 / 1_000.0).round() as usize;
        let delay_samples = delay_frames
            .saturating_mul(block.format.channels as usize)
            .min(self.buffer.len());
        for sample in &mut block.samples {
            let input = *sample;
            let read = if delay_samples == 0 {
                input
            } else {
                let index = (self.cursor + self.buffer.len() - delay_samples) % self.buffer.len();
                self.buffer[index]
            };
            self.buffer[self.cursor] = input;
            self.cursor += 1;
            if self.cursor == self.buffer.len() {
                self.cursor = 0;
            }
            *sample = read;
        }
    }
}
