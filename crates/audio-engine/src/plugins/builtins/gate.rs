use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use serde_json::Value;

use crate::{
    audio::format::AudioBlock,
    error::Result,
    plugins::{
        builtins::dsp_common::{
            api_version, audio_in_out_ports, db_to_linear, json_number, number_from, number_param,
            require_stereo, time_coeff, INTERNAL_SR,
        },
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::{PluginAudioRequirements, PluginDescriptor, PluginExecutionMode},
        process_protocol::PluginStats,
    },
};

pub const GATE_PLUGIN_ID: &str = "audio-router.builtin.gate";
const RMS_WINDOW_MS: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Closed,
    Open,
}

#[derive(Clone)]
struct ChannelGate {
    rms_squares: Vec<f32>,
    rms_index: usize,
    rms_sum: f64,
    rms_filled: usize,
    envelope: f32,
    hold_left: u32,
    open: bool,
}

impl ChannelGate {
    fn new(window: usize) -> Self {
        Self {
            rms_squares: vec![0.0; window.max(1)],
            rms_index: 0,
            rms_sum: 0.0,
            rms_filled: 0,
            envelope: 0.0,
            hold_left: 0,
            open: false,
        }
    }

    fn reset(&mut self) {
        self.rms_squares.fill(0.0);
        self.rms_index = 0;
        self.rms_sum = 0.0;
        self.rms_filled = 0;
        self.envelope = 0.0;
        self.hold_left = 0;
        self.open = false;
    }
}

pub struct NoiseGate {
    sample_rate: u32,
    threshold_db: f32,
    attack_ms: f32,
    hold_ms: f32,
    release_ms: f32,
    attack_coeff: f32,
    release_coeff: f32,
    hold_samples: u32,
    threshold_lin: f32,
    channels: Vec<ChannelGate>,
    smoothing: bool,
    open: Arc<AtomicBool>,
}

impl NoiseGate {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let sr = sample_rate.max(1);
        let window = rms_window(sr);
        let mut this = Self {
            sample_rate: sr,
            threshold_db: -50.0,
            attack_ms: 5.0,
            hold_ms: 50.0,
            release_ms: 100.0,
            attack_coeff: 1.0,
            release_coeff: 1.0,
            hold_samples: 1,
            threshold_lin: db_to_linear(-50.0),
            channels: vec![ChannelGate::new(window); channels.max(1)],
            smoothing: true,
            open: Arc::new(AtomicBool::new(false)),
        };
        this.recompute();
        this
    }

    pub fn open_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.open)
    }

    pub fn set_smoothing(&mut self, enabled: bool) {
        self.smoothing = enabled;
        self.recompute();
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        let window = rms_window(self.sample_rate);
        for ch in &mut self.channels {
            *ch = ChannelGate::new(window);
        }
        self.recompute();
    }

    pub fn set_channels(&mut self, channels: usize) {
        let n = channels.max(1);
        let window = rms_window(self.sample_rate);
        if self.channels.len() != n {
            self.channels = vec![ChannelGate::new(window); n];
        }
    }

    pub fn set_threshold_db(&mut self, value: f32) {
        self.threshold_db = value.clamp(-80.0, 0.0);
        self.recompute();
    }

    pub fn set_attack_ms(&mut self, value: f32) {
        self.attack_ms = value.clamp(0.1, 100.0);
        self.recompute();
    }

    pub fn set_hold_ms(&mut self, value: f32) {
        self.hold_ms = value.clamp(0.0, 1_000.0);
        self.recompute();
    }

    pub fn set_release_ms(&mut self, value: f32) {
        self.release_ms = value.clamp(10.0, 2_000.0);
        self.recompute();
    }

    pub fn state(&self) -> GateState {
        if self.open.load(Ordering::Relaxed) {
            GateState::Open
        } else {
            GateState::Closed
        }
    }

    pub fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset();
        }
        self.open.store(false, Ordering::Relaxed);
    }

    fn recompute(&mut self) {
        self.threshold_lin = db_to_linear(self.threshold_db);
        let attack = if self.smoothing { self.attack_ms } else { 0.1 };
        let release = if self.smoothing {
            self.release_ms
        } else {
            10.0
        };
        self.attack_coeff = time_coeff(attack, self.sample_rate);
        self.release_coeff = time_coeff(release, self.sample_rate);
        self.hold_samples = ((self.hold_ms / 1000.0) * self.sample_rate as f32).round() as u32;
    }

    pub fn process(&mut self, samples: &mut [f32], channels: usize) {
        self.set_channels(channels);
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        for frame in 0..frames {
            for ch in 0..channels {
                let index = frame * channels + ch;
                let x = samples[index];
                let gain = self.process_channel(ch, x);
                samples[index] = x * gain;
            }
        }
        let any_open = self.channels.iter().any(|ch| ch.envelope > 0.5);
        self.open.store(any_open, Ordering::Relaxed);
    }

    fn process_channel(&mut self, ch: usize, x: f32) -> f32 {
        let threshold = self.threshold_lin;
        let attack = self.attack_coeff;
        let release = self.release_coeff;
        let hold_samples = self.hold_samples;
        let slot = &mut self.channels[ch];
        let sq = x * x;
        let old = slot.rms_squares[slot.rms_index];
        slot.rms_sum += f64::from(sq) - f64::from(old);
        slot.rms_squares[slot.rms_index] = sq;
        slot.rms_index += 1;
        if slot.rms_index >= slot.rms_squares.len() {
            slot.rms_index = 0;
        }
        slot.rms_filled = (slot.rms_filled + 1).min(slot.rms_squares.len());
        let rms = (slot.rms_sum.max(0.0) / slot.rms_filled.max(1) as f64).sqrt() as f32;
        if rms >= threshold {
            slot.open = true;
            slot.hold_left = hold_samples;
            slot.envelope += (1.0 - slot.envelope) * attack;
        } else if slot.open {
            if slot.hold_left > 0 {
                slot.hold_left -= 1;
                slot.envelope += (1.0 - slot.envelope) * attack;
            } else {
                slot.envelope += (0.0 - slot.envelope) * release;
                if slot.envelope < 1.0e-4 {
                    slot.envelope = 0.0;
                    slot.open = false;
                }
            }
        } else {
            slot.envelope += (0.0 - slot.envelope) * release;
            if slot.envelope < 1.0e-4 {
                slot.envelope = 0.0;
            }
        }
        slot.envelope.clamp(0.0, 1.0)
    }
}

fn rms_window(sample_rate: u32) -> usize {
    ((RMS_WINDOW_MS / 1000.0) * sample_rate as f32)
        .round()
        .max(1.0) as usize
}

pub struct GateFactory;

impl GateFactory {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl PluginFactory for GateFactory {
    fn descriptor(&self) -> PluginDescriptor {
        let (input_ports, output_ports) = audio_in_out_ports();
        PluginDescriptor {
            plugin_id: GATE_PLUGIN_ID.into(),
            name: "ノイズゲート".into(),
            version: "1.0.0".into(),
            api_version: api_version(),
            category: "dynamics".into(),
            execution_mode: PluginExecutionMode::BuiltIn,
            input_ports,
            output_ports,
            parameters: vec![
                number_param(
                    "threshold_db",
                    "閾値 (Threshold)",
                    -50.0,
                    -80.0,
                    0.0,
                    "基本",
                    "dBFS",
                    0.1,
                    "このレベル未満を閉じます。",
                ),
                number_param(
                    "attack_ms",
                    "開く時間 (Attack)",
                    5.0,
                    0.1,
                    100.0,
                    "時間特性",
                    "ms",
                    0.1,
                    "開く速さです。",
                ),
                number_param(
                    "hold_ms",
                    "保持時間 (Hold)",
                    50.0,
                    0.0,
                    1_000.0,
                    "時間特性",
                    "ms",
                    1.0,
                    "閉動作に入るまでの保持です。",
                ),
                number_param(
                    "release_ms",
                    "閉じる時間 (Release)",
                    100.0,
                    10.0,
                    2_000.0,
                    "時間特性",
                    "ms",
                    1.0,
                    "閉じる速さです。",
                ),
            ],
            audio: PluginAudioRequirements::default(),
            reported_latency_frames: 0,
            default_enabled: false,
        }
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        Ok(Box::new(GateInstance::from_parameters(parameters)?))
    }
}

struct GateInstance {
    dsp: NoiseGate,
    open: Arc<AtomicBool>,
}

impl GateInstance {
    fn from_parameters(parameters: &BTreeMap<String, Value>) -> Result<Self> {
        let mut dsp = NoiseGate::new(INTERNAL_SR, 2);
        dsp.set_threshold_db(number_from(parameters, "threshold_db", -50.0)?);
        dsp.set_attack_ms(number_from(parameters, "attack_ms", 5.0)?);
        dsp.set_hold_ms(number_from(parameters, "hold_ms", 50.0)?);
        dsp.set_release_ms(number_from(parameters, "release_ms", 100.0)?);
        let open = dsp.open_flag();
        Ok(Self { dsp, open })
    }
}

impl TransformInstance for GateInstance {
    fn prepare(&mut self, context: PluginPrepareContext) -> Result<()> {
        require_stereo(context.channels)?;
        self.dsp.set_sample_rate(context.sample_rate);
        self.dsp.set_channels(context.channels as usize);
        self.dsp.reset();
        Ok(())
    }

    fn process(&mut self, block: &mut AudioBlock) -> Result<()> {
        self.dsp
            .process(&mut block.samples, block.format.channels as usize);
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: Value) -> Result<()> {
        let numeric = json_number(&value)?;
        match id {
            "threshold_db" => self.dsp.set_threshold_db(numeric),
            "attack_ms" => self.dsp.set_attack_ms(numeric),
            "hold_ms" => self.dsp.set_hold_ms(numeric),
            "release_ms" => self.dsp.set_release_ms(numeric),
            _ => {
                return Err(crate::error::BackendError::Plugin(format!(
                    "unknown gate parameter '{id}'"
                )))
            }
        }
        Ok(())
    }

    fn latest_stats(&self) -> Option<PluginStats> {
        Some(PluginStats {
            gate_open: Some(self.open.load(Ordering::Relaxed)),
            ..PluginStats::default()
        })
    }
}
