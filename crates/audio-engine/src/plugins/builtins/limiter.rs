use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use serde_json::{json, Value};

use crate::{
    audio::{
        format::{db_to_linear, AudioBlock, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
        graph::PortDirection,
    },
    error::{BackendError, Result},
    plugins::{
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::{
            ParameterDescriptor, ParameterKind, PluginAudioRequirements, PluginDescriptor,
            PluginExecutionMode, PluginPortDescriptor, TRANSFORM_PLUGIN_API_VERSION,
        },
    },
};

pub const LIMITER_PLUGIN_ID: &str = "audio-router.builtin.limiter";

pub struct LimiterFactory;

impl LimiterFactory {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl PluginFactory for LimiterFactory {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            plugin_id: LIMITER_PLUGIN_ID.into(),
            name: "Limiter".into(),
            version: "1.0.0".into(),
            api_version: TRANSFORM_PLUGIN_API_VERSION,
            category: "dynamics".into(),
            execution_mode: PluginExecutionMode::BuiltIn,
            input_ports: vec![PluginPortDescriptor {
                id: "audio_in".into(),
                name: "Audio In".into(),
                direction: PortDirection::Input,
                required: true,
                max_connections: Some(1),
            }],
            output_ports: vec![PluginPortDescriptor {
                id: "audio_out".into(),
                name: "Audio Out".into(),
                direction: PortDirection::Output,
                required: true,
                max_connections: None,
            }],
            parameters: vec![
                ParameterDescriptor {
                    id: "ceiling_db".into(),
                    label: "Ceiling".into(),
                    kind: ParameterKind::Number,
                    default: json!(-1.0),
                    min: Some(-24.0),
                    max: Some(0.0),
                    options: vec![],
                    description: "Maximum output peak in dBFS".into(),
                    group: "Limiter".into(),
                    unit: "dBFS".into(),
                    step: Some(0.1),
                    restart_required: false,
                },
                ParameterDescriptor {
                    id: "lookahead_ms".into(),
                    label: "Lookahead".into(),
                    kind: ParameterKind::Number,
                    default: json!(3.0),
                    min: Some(0.0),
                    max: Some(20.0),
                    options: vec![],
                    description: "Lookahead delay in milliseconds".into(),
                    group: "Limiter".into(),
                    unit: "ms".into(),
                    step: Some(0.1),
                    restart_required: false,
                },
                ParameterDescriptor {
                    id: "release_ms".into(),
                    label: "Release".into(),
                    kind: ParameterKind::Number,
                    default: json!(100.0),
                    min: Some(5.0),
                    max: Some(5000.0),
                    options: vec![],
                    description: "Gain recovery time in milliseconds".into(),
                    group: "Limiter".into(),
                    unit: "ms".into(),
                    step: Some(1.0),
                    restart_required: false,
                },
            ],
            audio: PluginAudioRequirements::default(),
            reported_latency_frames: 0,
            default_enabled: false,
        }
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        let ceiling_db = number(parameters, "ceiling_db", -1.0)?;
        let lookahead_ms = number(parameters, "lookahead_ms", 3.0)?;
        let release_ms = number(parameters, "release_ms", 100.0)?;
        Ok(Box::new(LimiterInstance::new(
            ceiling_db,
            lookahead_ms,
            release_ms,
        )?))
    }
}

fn number(parameters: &BTreeMap<String, Value>, id: &str, default: f32) -> Result<f32> {
    match parameters.get(id) {
        Some(v) => v.as_f64().map(|x| x as f32).ok_or_else(|| {
            BackendError::Plugin(format!("limiter parameter '{id}' must be numeric"))
        }),
        None => Ok(default),
    }
}

struct LimiterInstance {
    sample_rate: u32,
    ceiling_db: f32,
    lookahead_ms: f32,
    release_ms: f32,
    ceiling_linear: f32,
    release_step: f32,
    lookahead_frames: usize,
    delay: Vec<f32>,
    delay_index: usize,
    gain: f32,
    sequence: u64,
    peak_window: VecDeque<(u64, f32)>,
}

impl LimiterInstance {
    fn new(ceiling_db: f32, lookahead_ms: f32, release_ms: f32) -> Result<Self> {
        let mut this = Self {
            sample_rate: INTERNAL_SAMPLE_RATE,
            ceiling_db,
            lookahead_ms,
            release_ms,
            ceiling_linear: 1.0,
            release_step: 0.001,
            lookahead_frames: 0,
            delay: Vec::new(),
            delay_index: 0,
            gain: 1.0,
            sequence: 0,
            peak_window: VecDeque::new(),
        };
        this.reconfigure()?;
        Ok(this)
    }

    fn validate(&self) -> Result<()> {
        if !self.ceiling_db.is_finite() || !(-24.0..=0.0).contains(&self.ceiling_db) {
            return Err(BackendError::Plugin(
                "ceiling_db must be between -24 and 0".into(),
            ));
        }
        if !self.lookahead_ms.is_finite() || !(0.0..=20.0).contains(&self.lookahead_ms) {
            return Err(BackendError::Plugin(
                "lookahead_ms must be between 0 and 20".into(),
            ));
        }
        if !self.release_ms.is_finite() || !(5.0..=5000.0).contains(&self.release_ms) {
            return Err(BackendError::Plugin(
                "release_ms must be between 5 and 5000".into(),
            ));
        }
        Ok(())
    }

    fn reconfigure(&mut self) -> Result<()> {
        self.validate()?;
        self.ceiling_linear = db_to_linear(self.ceiling_db);
        self.lookahead_frames =
            ((self.lookahead_ms / 1000.0) * self.sample_rate as f32).round() as usize;
        let release_seconds = self.release_ms / 1000.0;
        self.release_step = 1.0 - (-1.0 / (release_seconds * self.sample_rate as f32)).exp();
        self.delay = vec![0.0; self.lookahead_frames.max(1) * INTERNAL_CHANNELS as usize];
        self.delay_index = 0;
        self.gain = 1.0;
        self.sequence = 0;
        self.peak_window.clear();
        Ok(())
    }
}

impl TransformInstance for LimiterInstance {
    fn prepare(&mut self, context: PluginPrepareContext) -> Result<()> {
        if context.channels != 2 {
            return Err(BackendError::Plugin(
                "built-in limiter currently requires stereo graph audio".into(),
            ));
        }
        self.sample_rate = context.sample_rate;
        self.reconfigure()
    }

    fn process(&mut self, block: &mut AudioBlock) -> Result<()> {
        if self.lookahead_frames == 0 {
            for frame in block.samples.chunks_exact_mut(2) {
                let peak = frame[0].abs().max(frame[1].abs());
                let target = if peak > self.ceiling_linear && peak > 0.0 {
                    self.ceiling_linear / peak
                } else {
                    1.0
                };
                if target < self.gain {
                    self.gain = target;
                } else {
                    self.gain += (1.0 - self.gain) * self.release_step;
                }
                frame[0] *= self.gain;
                frame[1] *= self.gain;
            }
            return Ok(());
        }

        for frame in block.samples.chunks_exact_mut(2) {
            let input_l = frame[0];
            let input_r = frame[1];
            let peak = input_l.abs().max(input_r.abs());
            let seq = self.sequence;
            self.sequence = self.sequence.wrapping_add(1);

            while self
                .peak_window
                .back()
                .map(|(_, p)| *p <= peak)
                .unwrap_or(false)
            {
                self.peak_window.pop_back();
            }
            self.peak_window.push_back((seq, peak));
            let oldest = seq.saturating_sub(self.lookahead_frames as u64);
            while self
                .peak_window
                .front()
                .map(|(i, _)| *i < oldest)
                .unwrap_or(false)
            {
                self.peak_window.pop_front();
            }
            let future_peak = self.peak_window.front().map(|(_, p)| *p).unwrap_or(0.0);
            let target = if future_peak > self.ceiling_linear && future_peak > 0.0 {
                self.ceiling_linear / future_peak
            } else {
                1.0
            };
            if target < self.gain {
                self.gain = target;
            } else {
                self.gain += (1.0 - self.gain) * self.release_step;
            }

            let base = self.delay_index * 2;
            let delayed_l = self.delay[base];
            let delayed_r = self.delay[base + 1];
            self.delay[base] = input_l;
            self.delay[base + 1] = input_r;
            self.delay_index += 1;
            if self.delay_index >= self.lookahead_frames {
                self.delay_index = 0;
            }

            frame[0] = delayed_l * self.gain;
            frame[1] = delayed_r * self.gain;
        }
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: Value) -> Result<()> {
        let numeric = value
            .as_f64()
            .ok_or_else(|| BackendError::Plugin(format!("parameter '{id}' must be numeric")))?
            as f32;
        match id {
            "ceiling_db" => self.ceiling_db = numeric,
            "lookahead_ms" => self.lookahead_ms = numeric,
            "release_ms" => self.release_ms = numeric,
            _ => {
                return Err(BackendError::Plugin(format!(
                    "unknown limiter parameter '{id}'"
                )))
            }
        }
        self.reconfigure()
    }

    fn latency_frames(&self) -> u32 {
        self.lookahead_frames as u32
    }
}
