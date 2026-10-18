use crate::audio::graph::{InputSelectorConfig, SwitcherManualResume, SwitcherMode};
use crate::audio::{format::AudioBlock, graph::PortId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    Startup,
    Manual,
    SilenceElapsed,
    IntervalElapsed,
    InputDisconnected,
    DefaultFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SwitchEvent {
    pub reason: SwitchReason,
    pub from_port_id: Option<PortId>,
    pub to_port_id: Option<PortId>,
    /// エンジン処理開始からの音声時間。壁時計や停止中の時間を含めない。
    pub at_seconds: f64,
    pub mode: SwitcherMode,
    pub threshold_db: f32,
    pub configured_seconds: f64,
    pub observed_silence_seconds: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct SwitcherSnapshot {
    pub selected_port_id: Option<PortId>,
    pub silence_seconds: f64,
    pub remaining_seconds: f64,
    pub automatic_paused: bool,
    pub last_switch: Option<SwitchEvent>,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameSelection {
    pub port_id: Option<PortId>,
    pub gain: f32,
}

#[derive(Default)]
pub struct SwitcherRuntime {
    initialized: bool,
    mode: SwitcherMode,
    configured_active: Option<PortId>,
    selection_revision: u64,
    resume_revision: u64,
    detected_for: Vec<(PortId, f64)>,
    elapsed: f64,
    interval_elapsed: f64,
    snapshot: SwitcherSnapshot,
    audible: Option<PortId>,
    gain: f32,
    target_gain: f32,
    fade_out_remaining: u64,
    fade_in_remaining: u64,
    fade_frames: u64,
}

impl SwitcherRuntime {
    /// Graph構築時に呼ぶ。音声コールバック中のadvance/next_frameは割当を行わない。
    pub fn prepare_ports(&mut self, ports: &[PortId]) {
        self.detected_for.retain(|(id, _)| ports.contains(id));
        for id in ports {
            if !self.detected_for.iter().any(|(existing, _)| existing == id) {
                self.detected_for.push((*id, 0.0));
            }
        }
    }

    pub fn snapshot(&self) -> SwitcherSnapshot {
        self.snapshot
    }
    pub fn reset(&mut self) {
        let ports = std::mem::take(&mut self.detected_for);
        *self = Self::default();
        self.detected_for = ports;
        self.detected_for
            .iter_mut()
            .for_each(|(_, time)| *time = 0.0);
    }
    pub fn resume(&mut self) {
        self.snapshot.automatic_paused = false;
        self.snapshot.silence_seconds = 0.0;
        self.interval_elapsed = 0.0;
    }

    /// levelsには接続済みPortだけを渡す。値は入力別ゲイン適用前のRMS。
    /// 一回の呼出し後にframes回next_frameを呼び、返された一Portのみを出力する。
    pub fn advance(
        &mut self,
        config: &InputSelectorConfig,
        ordered: &[PortId],
        levels: &[(PortId, f32)],
        frames: usize,
        sample_rate: u32,
    ) {
        if frames == 0 {
            return;
        }
        let dt = frames as f64 / sample_rate.max(1) as f64;
        self.elapsed += dt;
        let connected = |id| levels.iter().any(|(port, _)| *port == id);
        let priority = |id| {
            config
                .inputs
                .iter()
                .find(|p| p.port_id == id)
                .map_or(0, |p| p.priority)
        };
        let level_db = |id| {
            levels
                .iter()
                .find(|(port, _)| *port == id)
                .map_or(-240.0, |(_, rms)| 20.0 * rms.max(1.0e-12).log10())
        };
        for (port, time) in &mut self.detected_for {
            *time = if connected(*port) && level_db(*port) >= config.silence_threshold_db {
                *time + dt
            } else {
                0.0
            };
        }
        let first = || ordered.iter().copied().find(|id| connected(*id));
        let default = || config.default_port_id.filter(|id| connected(*id));
        let best = || {
            levels
                .iter()
                .map(|(id, _)| *id)
                .filter(|id| {
                    level_db(*id) >= config.silence_threshold_db
                        && self.detected_for.iter().any(|(port, time)| {
                            *port == *id && *time + 1e-9 >= config.detection_seconds
                        })
                })
                .min_by_key(|id| (priority(*id), *id))
        };
        let mut reason = None;
        let mut next = self.snapshot.selected_port_id;
        let mut immediate = false;
        if !self.initialized {
            next = config
                .active_port_id
                .filter(|id| connected(*id))
                .or_else(|| {
                    if config.active_port_id.is_some() {
                        None
                    } else if config.mode == SwitcherMode::RoundRobin {
                        levels
                            .iter()
                            .map(|(id, _)| *id)
                            .min_by_key(|id| (priority(*id), *id))
                    } else {
                        default().or_else(first)
                    }
                });
            reason = Some(SwitchReason::Startup);
            immediate = true;
            self.initialized = true;
        } else if config.active_port_id != self.configured_active
            || config.selection_revision != self.selection_revision
        {
            next = config
                .active_port_id
                .filter(|id| connected(*id))
                .or_else(|| {
                    if config.active_port_id.is_none() {
                        first()
                    } else {
                        None
                    }
                });
            reason = Some(SwitchReason::Manual);
            self.snapshot.automatic_paused = config.manual_resume == SwitcherManualResume::Paused;
            self.snapshot.silence_seconds = 0.0;
            self.interval_elapsed = 0.0;
        } else {
            if config.mode != self.mode || config.resume_revision != self.resume_revision {
                self.snapshot.automatic_paused = false;
                self.snapshot.silence_seconds = 0.0;
                self.interval_elapsed = 0.0;
            }
            if next.is_some_and(|id| !connected(id)) {
                next = if config.mode == SwitcherMode::VoiceActivity {
                    best().or_else(default)
                } else {
                    default().or_else(first)
                };
                reason = Some(SwitchReason::InputDisconnected);
            } else if !self.snapshot.automatic_paused {
                match config.mode {
                    SwitcherMode::Manual => {
                        if next.is_none() && config.active_port_id.is_none() {
                            next = first();
                            reason = Some(SwitchReason::Startup);
                        }
                    }
                    SwitcherMode::VoiceActivity => {
                        self.snapshot.silence_seconds =
                            if next.is_some_and(|id| level_db(id) >= config.silence_threshold_db) {
                                0.0
                            } else {
                                self.snapshot.silence_seconds + dt
                            };
                        if self.snapshot.silence_seconds + 1e-9 >= config.silence_seconds {
                            let candidate = best();
                            next = candidate.or_else(default);
                            reason = Some(if candidate.is_some() {
                                SwitchReason::SilenceElapsed
                            } else {
                                SwitchReason::DefaultFallback
                            });
                        }
                    }
                    SwitcherMode::RoundRobin => {
                        self.interval_elapsed += dt;
                        if self.interval_elapsed + 1e-9 >= config.interval_seconds.max(1.0e-6) {
                            let key = next.map(|id| (priority(id), id));
                            next = levels
                                .iter()
                                .map(|(id, _)| *id)
                                .filter(|id| key.is_none_or(|key| (priority(*id), *id) > key))
                                .min_by_key(|id| (priority(*id), *id))
                                .or_else(|| {
                                    levels
                                        .iter()
                                        .map(|(id, _)| *id)
                                        .min_by_key(|id| (priority(*id), *id))
                                });
                            self.interval_elapsed %= config.interval_seconds.max(1.0e-6);
                            reason = Some(SwitchReason::IntervalElapsed);
                        }
                    }
                }
            }
        }
        self.mode = config.mode;
        self.configured_active = config.active_port_id;
        self.selection_revision = config.selection_revision;
        self.resume_revision = config.resume_revision;
        if next != self.snapshot.selected_port_id {
            let from = self.snapshot.selected_port_id;
            self.snapshot.last_switch = Some(SwitchEvent {
                reason: reason.unwrap_or(SwitchReason::Startup),
                from_port_id: from,
                to_port_id: next,
                at_seconds: self.elapsed,
                mode: config.mode,
                threshold_db: config.silence_threshold_db,
                configured_seconds: if config.mode == SwitcherMode::RoundRobin {
                    config.interval_seconds
                } else {
                    config.silence_seconds
                },
                observed_silence_seconds: self.snapshot.silence_seconds,
            });
            self.snapshot.selected_port_id = next;
            self.snapshot.silence_seconds = 0.0;
            if reason != Some(SwitchReason::IntervalElapsed) {
                self.interval_elapsed = 0.0;
            }
            self.fade_frames =
                (config.fade_seconds.max(0.0) * sample_rate.max(1) as f64 / 2.0).round() as u64;
            if immediate || self.fade_frames == 0 {
                self.audible = next;
                self.fade_out_remaining = 0;
                self.fade_in_remaining = 0;
            } else {
                self.fade_out_remaining = self.fade_frames;
                self.fade_in_remaining = 0;
            }
        }
        self.target_gain = config
            .inputs
            .iter()
            .find(|p| Some(p.port_id) == next)
            .map_or(1.0, |p| 10.0_f32.powf(p.gain_db / 20.0));
        if self.fade_out_remaining == 0 {
            self.gain = self.target_gain;
        }
        self.snapshot.remaining_seconds = match config.mode {
            SwitcherMode::Manual => 0.0,
            SwitcherMode::VoiceActivity => {
                (config.silence_seconds - self.snapshot.silence_seconds).max(0.0)
            }
            SwitcherMode::RoundRobin => (config.interval_seconds - self.interval_elapsed).max(0.0),
        };
    }

    pub fn next_frame(&mut self) -> FrameSelection {
        if self.fade_out_remaining > 0 {
            self.fade_out_remaining -= 1;
            let frame = FrameSelection {
                port_id: self.audible,
                gain: self.gain * self.fade_out_remaining as f32 / self.fade_frames as f32,
            };
            if self.fade_out_remaining == 0 {
                self.audible = self.snapshot.selected_port_id;
                self.gain = self.target_gain;
                self.fade_in_remaining = self.fade_frames;
            }
            return frame;
        }
        if self.fade_in_remaining > 0 {
            self.fade_in_remaining -= 1;
            return FrameSelection {
                port_id: self.audible,
                gain: self.gain * (1.0 - self.fade_in_remaining as f32 / self.fade_frames as f32),
            };
        }
        FrameSelection {
            port_id: self.audible,
            gain: self.gain,
        }
    }
}

pub fn process(
    active_port_id: Option<PortId>,
    ordered_ports: &[PortId],
    inputs: &[(PortId, &AudioBlock)],
    output: &mut AudioBlock,
) -> Option<PortId> {
    output.clear();
    let selected = active_port_id.or_else(|| {
        ordered_ports.iter().find_map(|port_id| {
            inputs
                .iter()
                .any(|(input_port, _)| input_port == port_id)
                .then_some(*port_id)
        })
    })?;
    let input = inputs
        .iter()
        .find_map(|(port_id, input)| (*port_id == selected).then_some(*input))?;
    let count = input.samples.len().min(output.samples.len());
    output.samples[..count].copy_from_slice(&input.samples[..count]);
    Some(selected)
}
