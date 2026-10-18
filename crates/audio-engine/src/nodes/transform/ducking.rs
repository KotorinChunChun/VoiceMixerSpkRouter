use crate::audio::{format::db_to_linear, graph::DuckingConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuckingRuntimeState {
    Idle,
    Attacking,
    Ducking,
    Releasing,
}

pub struct DuckingProcessor {
    envelope_db: f32,
    state: DuckingRuntimeState,
    trigger_level_db: f32,
    trigger_detected: bool,
    gain_reduction_db: f32,
}

impl Default for DuckingProcessor {
    fn default() -> Self {
        Self {
            envelope_db: 0.0,
            state: DuckingRuntimeState::Idle,
            trigger_level_db: -240.0,
            trigger_detected: false,
            gain_reduction_db: 0.0,
        }
    }
}

impl DuckingProcessor {
    pub fn fill_frame_gains(
        &mut self,
        trigger: &[f32],
        sample_rate: u32,
        trigger_gain: f32,
        trigger_muted: bool,
        config: DuckingConfig,
        gains: &mut [f32],
    ) {
        let rms = if trigger_muted || trigger.is_empty() {
            0.0
        } else {
            let energy: f32 = trigger
                .iter()
                .map(|sample| (sample * trigger_gain).powi(2))
                .sum();
            (energy / trigger.len() as f32).sqrt()
        };
        let level_db = 20.0 * rms.max(1.0e-12).log10();
        self.trigger_level_db = level_db;
        self.trigger_detected = config.enabled && level_db >= config.threshold_db;
        let desired_db = if self.trigger_detected {
            config.reduction_db
        } else {
            0.0
        };
        let time_ms = if desired_db < self.envelope_db {
            config.attack_ms
        } else {
            config.release_ms
        };
        let coefficient = if time_ms <= 0.0 {
            0.0
        } else {
            (-1.0 / (time_ms * 0.001 * sample_rate as f32)).exp()
        };
        self.gain_reduction_db = 0.0;
        for gain in gains {
            self.envelope_db = desired_db + (self.envelope_db - desired_db) * coefficient;
            *gain = db_to_linear(self.envelope_db);
            self.gain_reduction_db = self.gain_reduction_db.max(-self.envelope_db);
        }
        self.state = if desired_db < 0.0 {
            if (self.envelope_db - desired_db).abs() < 0.1 {
                DuckingRuntimeState::Ducking
            } else {
                DuckingRuntimeState::Attacking
            }
        } else if self.envelope_db < -0.1 {
            DuckingRuntimeState::Releasing
        } else {
            DuckingRuntimeState::Idle
        };
    }

    pub fn state(&self) -> DuckingRuntimeState {
        self.state
    }

    pub fn trigger_level_db(&self) -> f32 {
        self.trigger_level_db
    }
    pub fn trigger_detected(&self) -> bool {
        self.trigger_detected
    }
    /// 直近ブロックで実際に適用した最大減衰量（正のdB）。
    pub fn gain_reduction_db(&self) -> f32 {
        self.gain_reduction_db
    }
}
