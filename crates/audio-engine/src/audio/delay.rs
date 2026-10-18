/// Fixed delay line used for latency compensation between parallel paths.
/// Storage is allocated only when a runtime plan is compiled, never per block.
pub struct DelayLine {
    buffer: Vec<f32>,
    cursor: usize,
}

impl DelayLine {
    pub fn new(delay_frames: usize, channels: usize) -> Self {
        Self {
            buffer: vec![0.0; delay_frames.saturating_mul(channels)],
            cursor: 0,
        }
    }

    pub fn delay_samples(&self) -> usize {
        self.buffer.len()
    }

    pub fn mix_into(&mut self, input: &[f32], output: &mut [f32], gain: f32) {
        let n = input.len().min(output.len());
        if self.buffer.is_empty() {
            for i in 0..n {
                output[i] += input[i] * gain;
            }
            return;
        }
        for i in 0..n {
            let delayed = self.buffer[self.cursor];
            self.buffer[self.cursor] = input[i];
            self.cursor += 1;
            if self.cursor == self.buffer.len() {
                self.cursor = 0;
            }
            output[i] += delayed * gain;
        }
    }

    pub fn mix_into_with_frame_gains(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        gain: f32,
        frame_gains: &[f32],
        channels: usize,
    ) {
        let n = input.len().min(output.len());
        if channels == 0 {
            return;
        }
        for i in 0..n {
            let delayed = if self.buffer.is_empty() {
                input[i]
            } else {
                let delayed = self.buffer[self.cursor];
                self.buffer[self.cursor] = input[i];
                self.cursor += 1;
                if self.cursor == self.buffer.len() {
                    self.cursor = 0;
                }
                delayed
            };
            output[i] += delayed * gain * frame_gains.get(i / channels).copied().unwrap_or(1.0);
        }
    }
}
