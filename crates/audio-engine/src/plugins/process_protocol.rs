//! Stable, serialization-only protocol types for out-of-process transform
//! plugins such as RVC. The audio engine itself does not depend on any RVC
//! implementation. The current process plugin host transports control messages over
//! stdin/stdout JSON Lines and PCM through shared-memory ring buffers without
//! changing graph code. The protocol types are transport-neutral enough to move
//! control IPC to named pipes in a future protocol version.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROCESS_PLUGIN_PROTOCOL_VERSION: u32 = 1;
pub const PROCESS_PLUGIN_PROTOCOL_VERSION_V2: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessPluginHello {
    pub protocol_version: u32,
    pub plugin_id: String,
    pub plugin_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedAudioTransport {
    pub input_mapping_name: String,
    pub output_mapping_name: String,
    pub capacity_frames: usize,
    pub sample_rate: u32,
    pub channels: u16,
    /// API v2 supplies one mapping name per port. v1 keeps these fields empty
    /// and continues using the two legacy mappings above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_port_mapping_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_port_mapping_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HostToPluginMessage {
    Prepare {
        sample_rate: u32,
        channels: u16,
        graph_block_frames: usize,
        transport: SharedAudioTransport,
    },
    Start,
    Stop,
    SetParameters(BTreeMap<String, Value>),
    Ports {
        connected: Vec<String>,
    },
    Ping {
        sequence: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum PluginToHostMessage {
    Hello(ProcessPluginHello),
    Ready {
        latency_frames: u32,
        preferred_block_frames: Option<usize>,
    },
    Latency {
        frames: u32,
    },
    State {
        state: String,
        message: Option<String>,
    },
    Pong {
        sequence: u64,
    },
    Error {
        code: String,
        message: String,
    },
    Stats(PluginStats),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, schemars::JsonSchema)]
pub struct PluginStats {
    #[serde(default)]
    pub inference_ms: Option<f64>,
    /// 初回推論を含む準備の実測時間。対応しないプラグインは省略する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warmup_ms: Option<f64>,
    #[serde(default)]
    pub queue_frames: Option<u32>,
    #[serde(default)]
    pub gpu_utilization: Option<f64>,
    #[serde(default)]
    pub gpu_memory_mb: Option<u32>,
    #[serde(default)]
    pub underruns: Option<u64>,
    #[serde(default)]
    pub gate_open: Option<bool>,
    #[serde(default)]
    pub gain_reduction_db: Option<f64>,
    /// ダッキングDSPが実際に検知したブロックRMS。未接続・未計測はNone。
    #[serde(default)]
    pub sidechain_level_db: Option<f64>,
    /// 有効かつDSP閾値以上のときtrue。ゲイン低下量とは独立する。
    #[serde(default)]
    pub sidechain_detected: Option<bool>,
    #[serde(default)]
    pub nr_enabled: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum PluginControlLine {
    Message(PluginToHostMessage),
    Ignored,
    InvalidJson(String),
}

pub fn decode_plugin_control_line(line: &str) -> PluginControlLine {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => return PluginControlLine::InvalidJson(error.to_string()),
    };
    let Some(message_type) = value.get("type").and_then(Value::as_str) else {
        return PluginControlLine::InvalidJson("missing type".into());
    };
    match message_type {
        "stats" => {
            let payload = value.get("payload").cloned().unwrap_or(Value::Null);
            match serde_json::from_value::<PluginStats>(payload) {
                Ok(stats) => PluginControlLine::Message(PluginToHostMessage::Stats(stats)),
                Err(_) => PluginControlLine::Ignored,
            }
        }
        "hello" | "ready" | "state" | "pong" | "error" | "latency" => {
            match serde_json::from_value(value) {
                Ok(message) => PluginControlLine::Message(message),
                Err(error) => PluginControlLine::InvalidJson(error.to_string()),
            }
        }
        _ => PluginControlLine::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_plugin_control_line, HostToPluginMessage, PluginControlLine, PluginToHostMessage,
    };

    #[test]
    fn t03_u_05_stats_json_parses_optional_and_unknown_fields() {
        let full = decode_plugin_control_line(
            r#"{"type":"stats","payload":{"inference_ms":12.4,"prepare_ms":26000.0,"warmup_ms":2200.0,"queue_frames":960,"gpu_utilization":0.63,"gpu_memory_mb":2048,"underruns":3}}"#,
        );
        match full {
            PluginControlLine::Message(PluginToHostMessage::Stats(stats)) => {
                assert_eq!(stats.inference_ms, Some(12.4));
                assert_eq!(stats.prepare_ms, Some(26000.0));
                assert_eq!(stats.warmup_ms, Some(2200.0));
                assert_eq!(stats.queue_frames, Some(960));
                assert_eq!(stats.gpu_utilization, Some(0.63));
                assert_eq!(stats.gpu_memory_mb, Some(2048));
                assert_eq!(stats.underruns, Some(3));
            }
            other => panic!("expected stats, got {other:?}"),
        }

        let partial =
            decode_plugin_control_line(r#"{"type":"stats","payload":{"inference_ms":8.0}}"#);
        match partial {
            PluginControlLine::Message(PluginToHostMessage::Stats(stats)) => {
                assert_eq!(stats.inference_ms, Some(8.0));
                assert!(stats.queue_frames.is_none());
                assert!(stats.prepare_ms.is_none());
                assert!(stats.warmup_ms.is_none());
            }
            other => panic!("expected partial stats, got {other:?}"),
        }

        let unknown = decode_plugin_control_line(
            r#"{"type":"stats","payload":{"inference_ms":1.0,"future_field":true}}"#,
        );
        match unknown {
            PluginControlLine::Message(PluginToHostMessage::Stats(stats)) => {
                assert_eq!(stats.inference_ms, Some(1.0));
            }
            other => panic!("expected stats with unknown field ignored, got {other:?}"),
        }

        let invalid =
            decode_plugin_control_line(r#"{"type":"stats","payload":{"inference_ms":"bad"}}"#);
        assert!(matches!(invalid, PluginControlLine::Ignored));

        let future_type = decode_plugin_control_line(r#"{"type":"future_thing","payload":{}}"#);
        assert!(matches!(future_type, PluginControlLine::Ignored));

        let latency = decode_plugin_control_line(
            r#"{"type":"latency","payload":{"frames":1920,"future_field":true}}"#,
        );
        assert!(matches!(
            latency,
            PluginControlLine::Message(PluginToHostMessage::Latency { frames: 1920 })
        ));

        let ports = serde_json::to_value(HostToPluginMessage::Ports {
            connected: vec!["in_main".into(), "in_sidechain".into()],
        })
        .unwrap();
        assert_eq!(ports["type"], "ports");
        assert_eq!(ports["payload"]["connected"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn t07_u_06_v2_transport_metadata_round_trips_and_v1_defaults() {
        let transport = super::SharedAudioTransport {
            input_mapping_name: "in.map".into(),
            output_mapping_name: "out.map".into(),
            capacity_frames: 480,
            sample_rate: 48_000,
            channels: 2,
            layout_version: Some(2),
            input_port_mapping_names: vec!["in.map".into()],
            output_port_mapping_names: vec!["out.map".into()],
        };
        let encoded = serde_json::to_value(&transport).unwrap();
        let decoded: super::SharedAudioTransport = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.layout_version, Some(2));
        assert_eq!(decoded.input_port_mapping_names, vec!["in.map"]);
        let v1: super::SharedAudioTransport = serde_json::from_value(serde_json::json!({
            "input_mapping_name": "in.map", "output_mapping_name": "out.map",
            "capacity_frames": 480, "sample_rate": 48000, "channels": 2
        }))
        .unwrap();
        assert_eq!(v1.layout_version, None);
        assert!(v1.input_port_mapping_names.is_empty());
    }
}
