use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use nnnoiseless::DenoiseState;
use serde_json::Value;

use crate::{
    audio::format::AudioBlock,
    error::Result,
    plugins::{
        builtins::dsp_common::{
            api_version, audio_in_out_ports, bool_from, bool_param, json_bool, json_number,
            number_from, number_param, require_stereo, SmoothLin, INTERNAL_SR,
        },
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::{PluginAudioRequirements, PluginDescriptor, PluginExecutionMode},
        process_protocol::PluginStats,
    },
};

pub const NOISE_SUPPRESSOR_PLUGIN_ID: &str = "audio-router.builtin.noise_suppressor";
const FRAME: usize = DenoiseState::FRAME_SIZE;
const PCM_SCALE: f32 = 32768.0;

struct ChannelNr {
    denoise: Box<DenoiseState<'static>>,
    in_buf: Vec<f32>,
    in_fill: usize,
    out_ring: Vec<f32>,
    out_read: usize,
    out_len: usize,
}

impl ChannelNr {
    fn new() -> Self {
        Self {
            denoise: DenoiseState::new(),
            in_buf: vec![0.0; FRAME],
            in_fill: 0,
            out_ring: vec![0.0; FRAME],
            out_read: 0,
            out_len: 0,
        }
    }

    fn reset(&mut self) {
        self.denoise = DenoiseState::new();
        self.in_buf.fill(0.0);
        self.in_fill = 0;
        self.out_ring.fill(0.0);
        self.out_read = 0;
        self.out_len = 0;
    }

    fn push_sample(&mut self, sample: f32) -> f32 {
        self.in_buf[self.in_fill] = sample * PCM_SCALE;
        self.in_fill += 1;
        if self.in_fill == FRAME {
            let mut out = [0.0f32; FRAME];
            self.denoise.process_frame(&mut out, &self.in_buf);
            for (dst, src) in self.out_ring.iter_mut().zip(out.iter()) {
                *dst = src / PCM_SCALE;
            }
            self.in_fill = 0;
            self.out_read = 0;
            self.out_len = FRAME;
        }
        if self.out_len == 0 {
            return 0.0;
        }
        let y = self.out_ring[self.out_read];
        self.out_read += 1;
        self.out_len -= 1;
        y
    }
}

pub struct NoiseSuppressor {
    sample_rate: u32,
    enabled: bool,
    strength: f32,
    mix: SmoothLin,
    enabled_gain: SmoothLin,
    channels: Vec<ChannelNr>,
    enabled_flag: Arc<AtomicBool>,
}

impl NoiseSuppressor {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let sr = sample_rate.max(1);
        Self {
            sample_rate: sr,
            enabled: true,
            strength: 0.4,
            mix: SmoothLin::new(0.4, sr),
            enabled_gain: SmoothLin::new(1.0, sr),
            channels: (0..channels.max(1)).map(|_| ChannelNr::new()).collect(),
            enabled_flag: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn enabled_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.enabled_flag)
    }

    pub fn set_smoothing(&mut self, enabled: bool) {
        self.mix.set_enabled(enabled);
        self.enabled_gain.set_enabled(enabled);
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.mix.set_sample_rate(self.sample_rate);
        self.enabled_gain.set_sample_rate(self.sample_rate);
    }

    pub fn set_channels(&mut self, channels: usize) {
        let n = channels.max(1);
        if self.channels.len() != n {
            self.channels = (0..n).map(|_| ChannelNr::new()).collect();
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.enabled_flag.store(enabled, Ordering::Relaxed);
        self.enabled_gain
            .set_target(if enabled { 1.0 } else { 0.0 });
    }

    pub fn set_strength(&mut self, percent: f32) {
        self.strength = percent.clamp(0.0, 100.0) / 100.0;
        self.mix.set_target(self.strength);
    }

    pub fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset();
        }
        self.mix.snap(self.strength);
        self.enabled_gain.snap(if self.enabled { 1.0 } else { 0.0 });
    }

    pub fn latency_frames(&self) -> u32 {
        if self.enabled {
            FRAME as u32
        } else {
            0
        }
    }

    pub fn process(&mut self, samples: &mut [f32], channels: usize) {
        self.set_channels(channels);
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        for frame in 0..frames {
            let mix = self.mix.next();
            let wet_enable = self.enabled_gain.next();
            for ch in 0..channels {
                let index = frame * channels + ch;
                let dry = samples[index];
                let wet = self.channels[ch].push_sample(dry);
                let mixed = wet * mix + dry * (1.0 - mix);
                samples[index] = mixed * wet_enable + dry * (1.0 - wet_enable);
            }
        }
    }
}

pub struct NoiseSuppressorFactory;

impl NoiseSuppressorFactory {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl PluginFactory for NoiseSuppressorFactory {
    fn descriptor(&self) -> PluginDescriptor {
        let (input_ports, output_ports) = audio_in_out_ports();
        PluginDescriptor {
            plugin_id: NOISE_SUPPRESSOR_PLUGIN_ID.into(),
            name: "ノイズ抑制".into(),
            version: "1.0.0".into(),
            api_version: api_version(),
            category: "restore".into(),
            execution_mode: PluginExecutionMode::BuiltIn,
            input_ports,
            output_ports,
            parameters: vec![
                bool_param(
                    "enabled",
                    "有効",
                    true,
                    "基本",
                    "RNNoiseによるノイズ抑制です。",
                ),
                number_param(
                    "strength",
                    "抑制強度",
                    40.0,
                    0.0,
                    100.0,
                    "基本",
                    "%",
                    1.0,
                    "処理後signalと原音のドライミックス率です。",
                ),
            ],
            audio: PluginAudioRequirements::default(),
            reported_latency_frames: 0,
            default_enabled: false,
        }
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        Ok(Box::new(NoiseSuppressorInstance::from_parameters(
            parameters,
        )?))
    }
}

struct NoiseSuppressorInstance {
    dsp: NoiseSuppressor,
    enabled: Arc<AtomicBool>,
}

impl NoiseSuppressorInstance {
    fn from_parameters(parameters: &BTreeMap<String, Value>) -> Result<Self> {
        let mut dsp = NoiseSuppressor::new(INTERNAL_SR, 2);
        dsp.set_enabled(bool_from(parameters, "enabled", true)?);
        dsp.set_strength(number_from(parameters, "strength", 40.0)?);
        let enabled = dsp.enabled_flag();
        Ok(Self { dsp, enabled })
    }
}

impl TransformInstance for NoiseSuppressorInstance {
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
        match id {
            "enabled" => self.dsp.set_enabled(json_bool(&value)?),
            "strength" => self.dsp.set_strength(json_number(&value)?),
            _ => {
                return Err(crate::error::BackendError::Plugin(format!(
                    "unknown noise suppressor parameter '{id}'"
                )))
            }
        }
        Ok(())
    }

    fn latency_frames(&self) -> u32 {
        self.dsp.latency_frames()
    }

    fn latest_stats(&self) -> Option<PluginStats> {
        Some(PluginStats {
            nr_enabled: Some(self.enabled.load(Ordering::Relaxed)),
            ..PluginStats::default()
        })
    }
}
