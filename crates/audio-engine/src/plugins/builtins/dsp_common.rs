use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::{
    audio::graph::PortDirection,
    error::{BackendError, Result},
    plugins::manifest::{
        ParameterDescriptor, ParameterKind, PluginPortDescriptor, TRANSFORM_PLUGIN_API_VERSION,
    },
};

pub const GAIN_SMOOTH_MS: f32 = 5.0;
pub const INTERNAL_SR: u32 = 48_000;

pub fn audio_in_out_ports() -> (Vec<PluginPortDescriptor>, Vec<PluginPortDescriptor>) {
    (
        vec![PluginPortDescriptor {
            id: "audio_in".into(),
            name: "Audio In".into(),
            direction: PortDirection::Input,
            required: true,
            max_connections: Some(1),
        }],
        vec![PluginPortDescriptor {
            id: "audio_out".into(),
            name: "Audio Out".into(),
            direction: PortDirection::Output,
            required: true,
            max_connections: None,
        }],
    )
}

pub fn api_version() -> u32 {
    TRANSFORM_PLUGIN_API_VERSION
}

pub fn number_param(
    id: &str,
    label: &str,
    default: f64,
    min: f64,
    max: f64,
    group: &str,
    unit: &str,
    step: f64,
    description: &str,
) -> ParameterDescriptor {
    ParameterDescriptor {
        id: id.into(),
        label: label.into(),
        kind: ParameterKind::Number,
        default: json!(default),
        min: Some(min),
        max: Some(max),
        options: vec![],
        description: description.into(),
        group: group.into(),
        unit: unit.into(),
        step: Some(step),
        restart_required: false,
    }
}

pub fn bool_param(
    id: &str,
    label: &str,
    default: bool,
    group: &str,
    description: &str,
) -> ParameterDescriptor {
    ParameterDescriptor {
        id: id.into(),
        label: label.into(),
        kind: ParameterKind::Boolean,
        default: json!(default),
        min: None,
        max: None,
        options: vec![],
        description: description.into(),
        group: group.into(),
        unit: "On/Off".into(),
        step: Some(1.0),
        restart_required: false,
    }
}

pub fn number_from(parameters: &BTreeMap<String, Value>, id: &str, default: f32) -> Result<f32> {
    match parameters.get(id) {
        Some(value) => value
            .as_f64()
            .map(|x| x as f32)
            .ok_or_else(|| BackendError::Plugin(format!("parameter '{id}' must be numeric"))),
        None => Ok(default),
    }
}

pub fn bool_from(parameters: &BTreeMap<String, Value>, id: &str, default: bool) -> Result<bool> {
    match parameters.get(id) {
        Some(Value::Bool(v)) => Ok(*v),
        Some(value) => value
            .as_f64()
            .map(|x| x != 0.0)
            .ok_or_else(|| BackendError::Plugin(format!("parameter '{id}' must be boolean"))),
        None => Ok(default),
    }
}

pub fn json_number(value: &Value) -> Result<f32> {
    value
        .as_f64()
        .map(|x| x as f32)
        .ok_or_else(|| BackendError::Plugin("parameter must be numeric".into()))
}

pub fn json_bool(value: &Value) -> Result<bool> {
    match value {
        Value::Bool(v) => Ok(*v),
        other => other
            .as_f64()
            .map(|x| x != 0.0)
            .ok_or_else(|| BackendError::Plugin("parameter must be boolean".into())),
    }
}

pub fn linear_to_db(linear: f32) -> f32 {
    20.0 * linear.max(1.0e-12).log10()
}

pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

pub fn time_coeff(time_ms: f32, sample_rate: u32) -> f32 {
    let samples = (time_ms / 1000.0) * sample_rate as f32;
    if samples <= 1.0 {
        1.0
    } else {
        1.0 - (-1.0 / samples).exp()
    }
}

/// Linear ramp toward a target over `GAIN_SMOOTH_MS`.
#[derive(Debug, Clone)]
pub struct SmoothLin {
    current: f32,
    target: f32,
    remaining: u32,
    sample_rate: u32,
    enabled: bool,
}

impl SmoothLin {
    pub fn new(value: f32, sample_rate: u32) -> Self {
        Self {
            current: value,
            target: value,
            remaining: 0,
            sample_rate: sample_rate.max(1),
            enabled: true,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.current = self.target;
            self.remaining = 0;
        }
    }

    pub fn set_target(&mut self, target: f32) {
        if !target.is_finite() {
            return;
        }
        if (target - self.target).abs() < 1.0e-12 && (target - self.current).abs() < 1.0e-12 {
            return;
        }
        self.target = target;
        if !self.enabled {
            self.current = target;
            self.remaining = 0;
            return;
        }
        let n = ((self.sample_rate as f32) * GAIN_SMOOTH_MS / 1000.0).round() as u32;
        self.remaining = n.max(1);
    }

    pub fn next(&mut self) -> f32 {
        if self.remaining == 0 {
            self.current = self.target;
            return self.current;
        }
        let delta = (self.target - self.current) / self.remaining as f32;
        self.current += delta;
        self.remaining -= 1;
        self.current
    }

    pub fn snap(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.remaining = 0;
    }

    pub fn current(&self) -> f32 {
        self.current
    }
}

pub fn require_stereo(channels: u16) -> Result<()> {
    if channels != 2 {
        return Err(BackendError::Plugin(
            "built-in DSP currently requires stereo graph audio".into(),
        ));
    }
    Ok(())
}
