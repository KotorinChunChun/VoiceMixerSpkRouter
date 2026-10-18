use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use crate::audio::{format::AudioBlock, graph::SilenceDetectorConfig};
use crate::event_graph::SilenceTransitionKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SilenceDetectorState {
    Monitoring,
    CandidateSilence,
    Silent,
    CandidateResume,
    Cooldown,
    Disabled,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SilenceDetectorStateSnapshot {
    pub state: SilenceDetectorState,
    pub level_db: f32,
    pub silent_duration_ms: u64,
    pub dropped_events: u64,
}

pub struct AtomicSilenceDetectorState {
    state: AtomicU8,
    level_bits: AtomicU32,
    silent_duration_ms: AtomicU64,
    dropped_events: AtomicU64,
}

impl Default for AtomicSilenceDetectorState {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(state_to_u8(SilenceDetectorState::Monitoring)),
            level_bits: AtomicU32::new((-120.0_f32).to_bits()),
            silent_duration_ms: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
        }
    }
}

impl AtomicSilenceDetectorState {
    pub fn store(&self, state: SilenceDetectorState, level_db: f32, silent_duration_ms: u64) {
        self.level_bits.store(level_db.to_bits(), Ordering::Release);
        self.silent_duration_ms
            .store(silent_duration_ms, Ordering::Release);
        self.state.store(state_to_u8(state), Ordering::Release);
    }

    pub fn increment_dropped(&self) {
        self.dropped_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> SilenceDetectorStateSnapshot {
        SilenceDetectorStateSnapshot {
            state: state_from_u8(self.state.load(Ordering::Acquire)),
            level_db: f32::from_bits(self.level_bits.load(Ordering::Acquire)),
            silent_duration_ms: self.silent_duration_ms.load(Ordering::Acquire),
            dropped_events: self.dropped_events.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceTransition {
    pub kind: SilenceTransitionKind,
    pub level_db: f32,
    pub silent_duration_ms: u64,
}

pub struct SilenceDetectorProcessor {
    state: SilenceDetectorState,
    candidate_frames: u64,
    silent_frames: u64,
    cooldown_frames: u64,
    last_config: Option<SilenceDetectorConfig>,
}

impl Default for SilenceDetectorProcessor {
    fn default() -> Self {
        Self {
            state: SilenceDetectorState::Monitoring,
            candidate_frames: 0,
            silent_frames: 0,
            cooldown_frames: 0,
            last_config: None,
        }
    }
}

impl SilenceDetectorProcessor {
    pub fn process(
        &mut self,
        block: &AudioBlock,
        config: SilenceDetectorConfig,
    ) -> Option<SilenceTransition> {
        if self.last_config != Some(config) {
            self.reset();
            self.last_config = Some(config);
        }
        let frames = block.frames() as u64;
        let level_db = rms_db(&block.samples);
        let sample_rate = block.format.sample_rate as u64;
        let duration_frames = ms_to_frames(config.duration_ms, sample_rate);
        let cooldown_limit = ms_to_frames(config.cooldown_ms, sample_rate);
        let low = level_db <= config.threshold_db;
        let resumed = level_db >= config.resume_threshold_db;
        let mut transition = None;

        match self.state {
            SilenceDetectorState::Monitoring => {
                if low {
                    self.candidate_frames = frames;
                    self.state = SilenceDetectorState::CandidateSilence;
                    if self.candidate_frames >= duration_frames {
                        self.silent_frames = self.candidate_frames;
                        self.state = SilenceDetectorState::Silent;
                        transition = Some(SilenceTransition {
                            kind: SilenceTransitionKind::Started,
                            level_db,
                            silent_duration_ms: frames_to_ms(self.silent_frames, sample_rate),
                        });
                    }
                }
            }
            SilenceDetectorState::CandidateSilence => {
                if low {
                    self.candidate_frames = self.candidate_frames.saturating_add(frames);
                    if self.candidate_frames >= duration_frames {
                        self.silent_frames = self.candidate_frames;
                        self.state = SilenceDetectorState::Silent;
                        transition = Some(SilenceTransition {
                            kind: SilenceTransitionKind::Started,
                            level_db,
                            silent_duration_ms: frames_to_ms(self.silent_frames, sample_rate),
                        });
                    }
                } else {
                    self.candidate_frames = 0;
                    self.state = SilenceDetectorState::Monitoring;
                }
            }
            SilenceDetectorState::Silent => {
                self.silent_frames = self.silent_frames.saturating_add(frames);
                if resumed {
                    self.state = SilenceDetectorState::CandidateResume;
                }
            }
            SilenceDetectorState::CandidateResume => {
                if resumed {
                    let silent_duration_ms = frames_to_ms(self.silent_frames, sample_rate);
                    self.cooldown_frames = cooldown_limit;
                    self.state = if cooldown_limit == 0 {
                        SilenceDetectorState::Monitoring
                    } else {
                        SilenceDetectorState::Cooldown
                    };
                    transition = Some(SilenceTransition {
                        kind: SilenceTransitionKind::Resumed,
                        level_db,
                        silent_duration_ms,
                    });
                    self.candidate_frames = 0;
                    self.silent_frames = 0;
                } else {
                    self.silent_frames = self.silent_frames.saturating_add(frames);
                    self.state = SilenceDetectorState::Silent;
                }
            }
            SilenceDetectorState::Cooldown => {
                self.cooldown_frames = self.cooldown_frames.saturating_sub(frames);
                if self.cooldown_frames == 0 {
                    self.state = SilenceDetectorState::Monitoring;
                }
            }
            SilenceDetectorState::Disabled | SilenceDetectorState::Error => self.reset(),
        }
        transition
    }

    pub fn snapshot(&self, level_db: f32, sample_rate: u32) -> SilenceDetectorStateSnapshot {
        SilenceDetectorStateSnapshot {
            state: self.state,
            level_db,
            silent_duration_ms: frames_to_ms(
                self.candidate_frames.max(self.silent_frames),
                sample_rate as u64,
            ),
            dropped_events: 0,
        }
    }

    pub fn state(&self) -> SilenceDetectorState {
        self.state
    }

    fn reset(&mut self) {
        self.state = SilenceDetectorState::Monitoring;
        self.candidate_frames = 0;
        self.silent_frames = 0;
        self.cooldown_frames = 0;
    }
}

pub fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let energy = samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
    20.0 * energy.sqrt().max(1.0e-6).log10()
}

fn ms_to_frames(ms: u64, sample_rate: u64) -> u64 {
    ms.saturating_mul(sample_rate).saturating_add(999) / 1_000
}

fn frames_to_ms(frames: u64, sample_rate: u64) -> u64 {
    frames.saturating_mul(1_000) / sample_rate.max(1)
}

fn state_to_u8(state: SilenceDetectorState) -> u8 {
    match state {
        SilenceDetectorState::Monitoring => 0,
        SilenceDetectorState::CandidateSilence => 1,
        SilenceDetectorState::Silent => 2,
        SilenceDetectorState::CandidateResume => 3,
        SilenceDetectorState::Cooldown => 4,
        SilenceDetectorState::Disabled => 5,
        SilenceDetectorState::Error => 6,
    }
}

fn state_from_u8(value: u8) -> SilenceDetectorState {
    match value {
        1 => SilenceDetectorState::CandidateSilence,
        2 => SilenceDetectorState::Silent,
        3 => SilenceDetectorState::CandidateResume,
        4 => SilenceDetectorState::Cooldown,
        5 => SilenceDetectorState::Disabled,
        6 => SilenceDetectorState::Error,
        _ => SilenceDetectorState::Monitoring,
    }
}
