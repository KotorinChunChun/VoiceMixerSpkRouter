use std::{collections::BTreeSet, path::PathBuf};

use schemars::{schema::RootSchema, schema_for};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    audio::{
        clock::{LatencyAccuracy, LatencySnapshot, LatencyStage},
        graph::{CaptureMethod, Edge, EdgeId, NodeId, PortId, RuntimeState},
        meter::MeterFrame,
    },
    config::model::{LatencyPreset, PresetValues},
    events::{BackendEvent, NodeRestartReason},
    plugins::process_protocol::PluginStats,
    recording::state::{RecorderState, RecorderStateSnapshot},
    registry::device_history::{AppDeviceId, DeviceConnectionState, KnownDevice},
    replay::state::{ReplayPlaybackState, ReplayStateSnapshot},
    routing::loop_detector::{LoopWarning, LoopWarningKind},
    windows::devices::DeviceDirection,
};

pub fn backend_event_schema() -> RootSchema {
    schema_for!(BackendEvent)
}

#[derive(Debug, Serialize)]
pub struct CommandContract {
    pub command: String,
    pub args_schema: RootSchema,
    pub result_schema: RootSchema,
}

impl CommandContract {
    pub fn new(
        command: impl Into<String>,
        args_schema: RootSchema,
        result_schema: RootSchema,
    ) -> Self {
        Self {
            command: command.into(),
            args_schema,
            result_schema,
        }
    }
}

#[cfg(feature = "tauri")]
pub fn backend_command_contracts() -> Vec<CommandContract> {
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct ResetWindowStateArgs {}

    let mut contracts = crate::commands::tauri_commands::bindings::generated_command_contracts();
    assert_eq!(
        contracts.len(),
        crate::commands::tauri_commands::bindings::GENERATED_BACKEND_COMMAND_COUNT
    );
    contracts.push(CommandContract::new(
        "reset_window_state",
        schema_for!(ResetWindowStateArgs),
        schema_for!(()),
    ));
    contracts.sort_by(|left, right| left.command.cmp(&right.command));
    assert_eq!(
        contracts.len(),
        225,
        "登録済みTauri Command契約数が変化しました"
    );
    assert!(
        contracts
            .windows(2)
            .all(|pair| pair[0].command != pair[1].command),
        "Tauri Command契約名が重複しています"
    );
    contracts
}

/// Frontend契約検証用に、BackendEventの全variantを1件ずつ生成する。
/// 値は決定的にして、生成物の差分が契約変更だけを表すようにする。
pub fn backend_event_fixtures() -> Vec<BackendEvent> {
    let node_id = NodeId(fixed_uuid(1));
    let other_node_id = NodeId(fixed_uuid(2));
    let from_port = PortId(fixed_uuid(3));
    let to_port = PortId(fixed_uuid(4));
    let app_device_id = AppDeviceId(fixed_uuid(5));
    let device = KnownDevice {
        app_device_id,
        endpoint_id: "fixture-endpoint".to_owned(),
        friendly_name: "契約テストデバイス".to_owned(),
        direction: DeviceDirection::Capture,
        connection_state: DeviceConnectionState::Connected,
        capture_enabled: true,
        monitor_enabled: false,
        display_order: 0,
        last_detected_unix_ms: 1_700_000_000_000,
        last_usage: Some("capture".to_owned()),
        comment: "generated fixture".to_owned(),
    };

    vec![
        BackendEvent::DeviceAdded {
            device: device.clone(),
        },
        BackendEvent::DeviceStateChanged {
            device,
            previous_state: Some(DeviceConnectionState::Disconnected),
        },
        BackendEvent::DeviceRemoved { app_device_id },
        BackendEvent::ProcessStateChanged {
            node_id,
            previous_state: Some(RuntimeState::Starting),
            state: RuntimeState::Running,
        },
        BackendEvent::EdgeTabStateChanged {
            node_id,
            previous_state: Some(RuntimeState::Disconnected),
            state: RuntimeState::Running,
            reconnected: true,
        },
        BackendEvent::FolderFilesChanged {
            node_id,
            files: vec![PathBuf::from("fixtures/sample.wav")],
        },
        BackendEvent::ReplayStateChanged {
            node_id,
            snapshot: ReplayStateSnapshot {
                state: ReplayPlaybackState::Playing,
                position_seconds: 1.25,
                duration_seconds: Some(10.0),
                eof: false,
                current_file: Some("fixtures/sample.wav".to_owned()),
            },
        },
        BackendEvent::ReplayPositionChanged {
            node_id,
            position_seconds: 1.25,
            duration_seconds: Some(10.0),
        },
        BackendEvent::RecordingStateChanged {
            node_id,
            snapshot: RecorderStateSnapshot {
                state: RecorderState::Recording,
                auto_record: false,
                session_active: true,
                input_available: true,
                manual_stop_override: false,
                pending_manual_start: false,
                duration_seconds: 2.5,
                current_path: Some(PathBuf::from("fixtures/recording.wav")),
                last_output: None,
                error: None,
            },
        },
        BackendEvent::RecordingDurationChanged {
            node_id,
            duration_seconds: 2.5,
        },
        BackendEvent::NodeRuntimeStateChanged {
            node_id,
            previous_state: Some(RuntimeState::Stopped),
            state: RuntimeState::Running,
        },
        BackendEvent::AudioLevelChanged {
            node_id,
            port_id: Some(from_port),
            meter: MeterFrame {
                peak: 0.5,
                rms: 0.25,
                clipped: false,
            },
        },
        BackendEvent::AudioClipping { node_id, peak: 1.1 },
        BackendEvent::RoutingChanged {
            revision: 7,
            edges: vec![Edge {
                id: EdgeId(fixed_uuid(6)),
                from_node: node_id,
                from_port,
                to_node: other_node_id,
                to_port,
                gain_db: -3.0,
                mute: false,
                color: None,
            }],
        },
        BackendEvent::LoopWarningChanged {
            warnings: vec![LoopWarning {
                kind: LoopWarningKind::ExplicitGraph,
                message: "fixture loop".to_owned(),
                nodes: vec![node_id, other_node_id],
            }],
        },
        BackendEvent::LatencyChanged {
            node_id,
            stage: LatencyStage::EndToEnd,
            snapshot: LatencySnapshot {
                last_ms: 12.0,
                ema_ms: 11.5,
                max_ms: 15.0,
            },
            accuracy: LatencyAccuracy::HostEstimated,
        },
        BackendEvent::PresetApplied {
            preset: LatencyPreset::Balanced,
            values: PresetValues {
                block_frames: 480,
                input_method_priority: vec![CaptureMethod::WasapiSharedEvent],
                render_exclusive: false,
                jitter_target_fill: 0.5,
            },
        },
        BackendEvent::PresetApplyFailed {
            failed_node: node_id,
            reason: "fixture failure".to_owned(),
            rolled_back: true,
        },
        BackendEvent::PluginStatsChanged {
            node_id,
            stats: PluginStats {
                inference_ms: Some(20.0),
                prepare_ms: None,
                warmup_ms: None,
                queue_frames: Some(480),
                gpu_utilization: Some(50.0),
                gpu_memory_mb: Some(1024),
                underruns: Some(1),
                gate_open: Some(true),
                gain_reduction_db: Some(-4.0),
                nr_enabled: Some(true),
                sidechain_level_db: Some(-20.0),
                sidechain_detected: Some(true),
            },
            inference_warning: false,
        },
        BackendEvent::NodeRestarting {
            node_id,
            reason: NodeRestartReason::PresetApply,
        },
    ]
}

pub fn backend_event_fixture_values() -> serde_json::Result<Vec<Value>> {
    backend_event_fixtures()
        .into_iter()
        .map(serde_json::to_value)
        .collect()
}

pub fn fixture_event_names() -> serde_json::Result<BTreeSet<String>> {
    backend_event_fixture_values()?
        .into_iter()
        .map(|fixture| {
            fixture
                .get("event")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| serde_json::Error::io(std::io::Error::other("event tag is missing")))
        })
        .collect()
}

fn fixed_uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_cover_every_backend_event_name_once() {
        let fixtures = backend_event_fixtures();
        let serialized_names = fixture_event_names().expect("fixture serialization must succeed");
        let declared_names = BackendEvent::EVENT_NAMES
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let runtime_names = fixtures
            .iter()
            .map(|event| event.event_name().to_owned())
            .collect::<BTreeSet<_>>();

        assert_eq!(fixtures.len(), BackendEvent::EVENT_NAMES.len());
        assert_eq!(serialized_names, declared_names);
        assert_eq!(runtime_names, declared_names);
    }

    #[cfg(feature = "tauri")]
    #[test]
    fn command_contracts_cover_every_registered_tauri_command_once() {
        let contracts = backend_command_contracts();
        let names = contracts
            .iter()
            .map(|contract| contract.command.as_str())
            .collect::<BTreeSet<_>>();

        assert_eq!(contracts.len(), 225);
        assert_eq!(names.len(), contracts.len());
        assert!(names.contains("frontend_snapshot"));
        assert!(names.contains("reset_window_state"));
    }
}
