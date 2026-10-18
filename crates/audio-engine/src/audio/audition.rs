//! 永続Graphと分離した単一試聴セッション。
use crate::audio::{
    engine::AudioSink,
    graph::{NodeId, PortId},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct AuditionTarget {
    pub node_id: NodeId,
    /// 指定なしは第一出力。入力指定時は実際のPortへの接続音を聴く。
    #[serde(default)]
    pub input_port: Option<PortId>,
    #[serde(default)]
    pub output_port: Option<PortId>,
    #[serde(default)]
    pub gain_db: f32,
    /// A/Bの比較基準入力。補正は試聴専用、Graphへ保存しない。
    #[serde(default)]
    pub comparison_input: Option<PortId>,
    #[serde(default)]
    pub match_rms: bool,
    /// 通常の配信・録音を継続する試聴。旧設定は従来の休止モード。
    #[serde(default)]
    pub continue_outputs: bool,
}

pub struct AuditionSession {
    pub target: AuditionTarget,
    pub sink: Arc<dyn AudioSink>,
    pub comparison: Arc<parking_lot::Mutex<crate::audio::comparison::ComparisonMeter>>,
    pub failed: std::sync::atomic::AtomicBool,
}
