use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use serde_json::Value;

use crate::{
    audio::format::AudioBlock,
    error::Result,
    plugins::{
        builtins::dsp_common::{
            api_version, audio_in_out_ports, db_to_linear, json_number, linear_to_db, number_from,
            number_param, require_stereo, time_coeff, SmoothLin, INTERNAL_SR,
        },
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::{PluginAudioRequirements, PluginDescriptor, PluginExecutionMode},
        process_protocol::PluginStats,
    },
};

pub const COMPRESSOR_PLUGIN_ID: &str = "audio-router.builtin.compressor";

#[derive(Clone)]
struct ChannelComp {
    envelope: f32,
}

impl ChannelComp {
    fn new() -> Self {
        Self { envelope: 1.0e-12 }
    }

    fn reset(&mut self) {
        self.envelope = 1.0e-12;
    }
}

pub struct Compressor {
    sample_rate: u32,
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    makeup_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup: SmoothLin,
    channels: Vec<ChannelComp>,
    smoothing: bool,
    gr_bits: Arc<AtomicU32>,
}

impl Compressor {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let sr = sample_rate.max(1);
        let mut this = Self {
            sample_rate: sr,
            threshold_db: -18.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: 0.0,
            attack_coeff: 1.0,
            release_coeff: 1.0,
            makeup: SmoothLin::new(1.0, sr),
            channels: vec![ChannelComp::new(); channels.max(1)],
            smoothing: true,
            gr_bits: Arc::new(AtomicU32::new(0.0f32.to_bits())),
        };
        this.recompute();
        this
    }

    pub fn gr_bits(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.gr_bits)
    }

    pub fn gain_reduction_db(&self) -> f32 {
        f32::from_bits(self.gr_bits.load(Ordering::Relaxed))
    }

    pub fn set_smoothing(&mut self, enabled: bool) {
        self.smoothing = enabled;
        self.makeup.set_enabled(enabled);
        self.recompute();
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        self.makeup.set_sample_rate(self.sample_rate);
        self.recompute();
    }

    pub fn set_channels(&mut self, channels: usize) {
        let n = channels.max(1);
        if self.channels.len() != n {
            self.channels = vec![ChannelComp::new(); n];
        }
    }

    pub fn set_threshold_db(&mut self, value: f32) {
        self.threshold_db = value.clamp(-60.0, 0.0);
    }

    pub fn set_ratio(&mut self, value: f32) {
        self.ratio = value.clamp(1.0, 20.0);
    }

    pub fn set_attack_ms(&mut self, value: f32) {
        self.attack_ms = value.clamp(0.1, 100.0);
        self.recompute();
    }

    pub fn set_release_ms(&mut self, value: f32) {
        self.release_ms = value.clamp(10.0, 2_000.0);
        self.recompute();
    }

    pub fn set_makeup_db(&mut self, value: f32) {
        self.makeup_db = value.clamp(0.0, 24.0);
        self.makeup.set_target(db_to_linear(self.makeup_db));
    }

    pub fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.reset();
        }
        self.makeup.snap(db_to_linear(self.makeup_db));
        self.gr_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
    }

    fn recompute(&mut self) {
        let attack = if self.smoothing { self.attack_ms } else { 0.1 };
        let release = if self.smoothing {
            self.release_ms
        } else {
            10.0
        };
        self.attack_coeff = time_coeff(attack, self.sample_rate);
        // Scale so a typical 12 dB GR recovery matches the displayed Release time.
        self.release_coeff = time_coeff((release * 2.0 / 3.0).max(1.0), self.sample_rate);
        self.makeup.set_target(db_to_linear(self.makeup_db));
    }

    pub fn process(&mut self, samples: &mut [f32], channels: usize) {
        self.set_channels(channels);
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        let mut max_gr = 0.0f32;
        for frame in 0..frames {
            let makeup = self.makeup.next();
            for ch in 0..channels {
                let index = frame * channels + ch;
                let x = samples[index];
                let (y, gr) = self.process_channel(ch, x, makeup);
                samples[index] = y;
                max_gr = max_gr.max(gr);
            }
        }
        self.gr_bits.store(max_gr.to_bits(), Ordering::Relaxed);
    }

    fn process_channel(&mut self, ch: usize, x: f32, makeup: f32) -> (f32, f32) {
        let peak = x.abs().max(1.0e-12);
        let slot = &mut self.channels[ch];
        if peak > slot.envelope {
            slot.envelope += (peak - slot.envelope) * self.attack_coeff;
        } else {
            slot.envelope += (peak - slot.envelope) * self.release_coeff;
        }
        let env_db = linear_to_db(slot.envelope);
        let gr_db = if env_db > self.threshold_db && self.ratio > 1.0 {
            (env_db - self.threshold_db) * (1.0 - 1.0 / self.ratio)
        } else {
            0.0
        };
        let gain = db_to_linear(-gr_db) * makeup;
        (x * gain, gr_db)
    }
}

pub struct CompressorFactory;

impl CompressorFactory {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl PluginFactory for CompressorFactory {
    fn descriptor(&self) -> PluginDescriptor {
        let (input_ports, output_ports) = audio_in_out_ports();
        PluginDescriptor {
            plugin_id: COMPRESSOR_PLUGIN_ID.into(),
            name: "コンプレッサー".into(),
            version: "1.0.0".into(),
            api_version: api_version(),
            category: "dynamics".into(),
            execution_mode: PluginExecutionMode::BuiltIn,
            input_ports,
            output_ports,
            parameters: vec![
                number_param(
                    "threshold_db",
                    "Threshold",
                    -18.0,
                    -60.0,
                    0.0,
                    "基本",
                    "dBFS",
                    0.1,
                    "圧縮を始めるレベルです。",
                ),
                number_param(
                    "ratio",
                    "Ratio",
                    1.0,
                    1.0,
                    20.0,
                    "基本",
                    ":1",
                    0.1,
                    "圧縮比です。",
                ),
                number_param(
                    "makeup_db",
                    "Make-up",
                    0.0,
                    0.0,
                    24.0,
                    "基本",
                    "dB",
                    0.1,
                    "出力ゲインです。5 msで滑らかに追従します。",
                ),
                number_param(
                    "attack_ms",
                    "Attack",
                    10.0,
                    0.1,
                    100.0,
                    "時間特性",
                    "ms",
                    0.1,
                    "ゲインを下げる速さです。",
                ),
                number_param(
                    "release_ms",
                    "Release",
                    100.0,
                    10.0,
                    2_000.0,
                    "時間特性",
                    "ms",
                    1.0,
                    "ゲインを戻す速さです。",
                ),
            ],
            audio: PluginAudioRequirements::default(),
            reported_latency_frames: 0,
            // 新規配置直後も原音を止めずに確認できるよう、ratio=1の
            // unity-gain設定で有効化する。
            default_enabled: true,
        }
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        Ok(Box::new(CompressorInstance::from_parameters(parameters)?))
    }
}

struct CompressorInstance {
    dsp: Compressor,
    gr_bits: Arc<AtomicU32>,
}

impl CompressorInstance {
    fn from_parameters(parameters: &BTreeMap<String, Value>) -> Result<Self> {
        let mut dsp = Compressor::new(INTERNAL_SR, 2);
        dsp.set_threshold_db(number_from(parameters, "threshold_db", -18.0)?);
        dsp.set_ratio(number_from(parameters, "ratio", 1.0)?);
        dsp.set_attack_ms(number_from(parameters, "attack_ms", 10.0)?);
        dsp.set_release_ms(number_from(parameters, "release_ms", 100.0)?);
        dsp.set_makeup_db(number_from(parameters, "makeup_db", 0.0)?);
        let gr_bits = dsp.gr_bits();
        Ok(Self { dsp, gr_bits })
    }
}

impl TransformInstance for CompressorInstance {
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
            "ratio" => self.dsp.set_ratio(numeric),
            "attack_ms" => self.dsp.set_attack_ms(numeric),
            "release_ms" => self.dsp.set_release_ms(numeric),
            "makeup_db" => self.dsp.set_makeup_db(numeric),
            _ => {
                return Err(crate::error::BackendError::Plugin(format!(
                    "unknown compressor parameter '{id}'"
                )))
            }
        }
        Ok(())
    }

    fn latest_stats(&self) -> Option<PluginStats> {
        Some(PluginStats {
            gain_reduction_db: Some(f64::from(f32::from_bits(
                self.gr_bits.load(Ordering::Relaxed),
            ))),
            ..PluginStats::default()
        })
    }
}
