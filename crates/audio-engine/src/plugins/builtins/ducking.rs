//! Built-in API v2 side-chain ducking reference implementation.

use std::{collections::BTreeMap, sync::Arc};

use serde_json::Value;

use crate::{
    audio::{format::AudioBlock, graph::DuckingConfig},
    error::Result,
    nodes::transform::ducking::DuckingProcessor,
    plugins::{
        builtins::dsp_common::{bool_from, bool_param, number_from, number_param, INTERNAL_SR},
        instance::{PluginPrepareContext, TransformInstance},
        manager::PluginFactory,
        manifest::{
            PluginAudioRequirements, PluginDescriptor, PluginExecutionMode, PluginPortDescriptor,
            TRANSFORM_PLUGIN_API_VERSION_V2,
        },
        process_protocol::PluginStats,
    },
};

pub const DUCKING_PLUGIN_ID: &str = "audio-router.builtin.ducking";

pub struct DuckingFactory;

impl DuckingFactory {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl PluginFactory for DuckingFactory {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            plugin_id: DUCKING_PLUGIN_ID.into(),
            name: "サイドチェーン・ダッキング".into(),
            version: "1.0.0".into(),
            api_version: TRANSFORM_PLUGIN_API_VERSION_V2,
            category: "dynamics".into(),
            execution_mode: PluginExecutionMode::BuiltIn,
            input_ports: vec![
                PluginPortDescriptor {
                    id: "main".into(),
                    name: "メイン入力".into(),
                    direction: crate::audio::graph::PortDirection::Input,
                    required: true,
                    max_connections: Some(1),
                },
                PluginPortDescriptor {
                    id: "sidechain".into(),
                    name: "サイドチェーン入力".into(),
                    direction: crate::audio::graph::PortDirection::Input,
                    required: false,
                    max_connections: Some(1),
                },
            ],
            output_ports: vec![PluginPortDescriptor {
                id: "audio_out".into(),
                name: "ダッキング出力".into(),
                direction: crate::audio::graph::PortDirection::Output,
                required: true,
                max_connections: None,
            }],
            parameters: vec![
                bool_param(
                    "enabled",
                    "有効",
                    true,
                    "ダッキング",
                    "サイドチェーン入力がしきい値を超えたとき、メイン入力を減衰させます。",
                ),
                number_param(
                    "threshold_db",
                    "しきい値",
                    -36.0,
                    -120.0,
                    0.0,
                    "ダッキング",
                    "dBFS",
                    0.1,
                    "このレベル以上のサイドチェーン入力で減衰を開始します。",
                ),
                number_param(
                    "reduction_db",
                    "減衰量",
                    -12.0,
                    -120.0,
                    0.0,
                    "ダッキング",
                    "dB",
                    0.1,
                    "メイン入力へ加える減衰量です。0 dBでは原音をそのまま通します。",
                ),
                number_param(
                    "attack_ms",
                    "アタック",
                    25.0,
                    0.0,
                    10_000.0,
                    "ダッキング",
                    "ms",
                    0.1,
                    "減衰を始める速さです。",
                ),
                number_param(
                    "release_ms",
                    "リリース",
                    250.0,
                    0.0,
                    10_000.0,
                    "ダッキング",
                    "ms",
                    0.1,
                    "減衰から原音へ戻る速さです。",
                ),
            ],
            audio: PluginAudioRequirements::default(),
            reported_latency_frames: 0,
            // sidechain未接続や新規配置でもメイン入力は常に通す。
            default_enabled: true,
        }
    }

    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>> {
        Ok(Box::new(DuckingInstance::new(parameters)?))
    }
}

struct DuckingInstance {
    processor: DuckingProcessor,
    config: DuckingConfig,
    sample_rate: u32,
    gains: Vec<f32>,
    sidechain_connected: Option<bool>,
    stats: Option<PluginStats>,
}

impl DuckingInstance {
    fn new(parameters: &BTreeMap<String, Value>) -> Result<Self> {
        let mut this = Self {
            processor: DuckingProcessor::default(),
            config: DuckingConfig {
                enabled: true,
                trigger_port_id: crate::audio::graph::PortId::new(),
                target_port_id: crate::audio::graph::PortId::new(),
                threshold_db: -36.0,
                // 旧保存データで値が欠けている場合の解釈は従来の0 dBを保持する。
                // 新規ノードはmanifest既定値をbuild_node_specで埋める。
                reduction_db: 0.0,
                attack_ms: 25.0,
                release_ms: 250.0,
            },
            sample_rate: INTERNAL_SR,
            gains: Vec::new(),
            sidechain_connected: None,
            stats: None,
        };
        this.apply_parameters(parameters)?;
        Ok(this)
    }

    fn apply_parameters(&mut self, parameters: &BTreeMap<String, Value>) -> Result<()> {
        self.config.enabled = bool_from(parameters, "enabled", self.config.enabled)?;
        self.config.threshold_db =
            number_from(parameters, "threshold_db", self.config.threshold_db)?;
        self.config.reduction_db =
            number_from(parameters, "reduction_db", self.config.reduction_db)?;
        self.config.attack_ms = number_from(parameters, "attack_ms", self.config.attack_ms)?;
        self.config.release_ms = number_from(parameters, "release_ms", self.config.release_ms)?;
        Ok(())
    }
}

impl TransformInstance for DuckingInstance {
    fn prepare(&mut self, context: PluginPrepareContext) -> Result<()> {
        self.sample_rate = context.sample_rate.max(1);
        self.gains.resize(context.graph_block_frames.max(1), 1.0);
        self.processor = DuckingProcessor::default();
        self.stats = None;
        Ok(())
    }

    fn process(&mut self, block: &mut AudioBlock) -> Result<()> {
        block.apply_gain_db(0.0);
        Ok(())
    }

    fn process_ports(
        &mut self,
        inputs: &mut [AudioBlock],
        outputs: &mut [AudioBlock],
    ) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        let Some(main) = inputs.first() else {
            outputs[0].clear();
            self.stats = None;
            return Ok(());
        };
        outputs[0].samples.copy_from_slice(&main.samples);
        outputs[0].copy_latency_from(main);
        let side = inputs
            .get(1)
            .map(|block| block.samples.as_slice())
            .unwrap_or(&[]);
        self.gains.resize(outputs[0].frames().max(1), 1.0);
        self.processor.fill_frame_gains(
            side,
            self.sample_rate,
            1.0,
            false,
            self.config,
            &mut self.gains,
        );
        let connected = self.sidechain_connected.unwrap_or(inputs.len() > 1);
        self.stats = Some(PluginStats {
            gain_reduction_db: Some(f64::from(self.processor.gain_reduction_db())),
            sidechain_level_db: connected.then(|| f64::from(self.processor.trigger_level_db())),
            sidechain_detected: connected.then(|| self.processor.trigger_detected()),
            ..PluginStats::default()
        });
        for (frame, gain) in outputs[0]
            .samples
            .chunks_exact_mut(outputs[0].format.channels as usize)
            .zip(self.gains.iter())
        {
            for sample in frame {
                *sample *= *gain;
            }
        }
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: Value) -> Result<()> {
        let mut parameters = BTreeMap::new();
        parameters.insert(id.to_string(), value);
        self.apply_parameters(&parameters)
    }

    fn set_connected_ports(&mut self, connected: &[String]) -> Result<()> {
        self.sidechain_connected = Some(connected.iter().any(|id| id == "sidechain"));
        self.stats = None;
        Ok(())
    }

    fn latest_stats(&self) -> Option<PluginStats> {
        self.stats.clone()
    }
}
