use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    audio::{
        clock::frames_to_ms,
        format::INTERNAL_SAMPLE_RATE,
        graph::{Edge, NodeId, NodeProperties, NodeType, PortId, RuntimeState},
        meter::MeterFrame,
    },
    plugins::process_protocol::PluginStats,
    recording::state::{RecorderState, RecorderStateSnapshot},
    registry::device_history::{AppDeviceId, DeviceConnectionState, KnownDevice},
    replay::state::{ReplayPlaybackState, ReplayStateSnapshot},
    routing::loop_detector::{detect_loop_warnings, LoopWarning},
    Backend,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(tag = "event", content = "data", rename_all = "kebab-case")]
pub enum BackendEvent {
    DeviceAdded {
        device: KnownDevice,
    },
    DeviceStateChanged {
        device: KnownDevice,
        previous_state: Option<DeviceConnectionState>,
    },
    DeviceRemoved {
        app_device_id: AppDeviceId,
    },
    ProcessStateChanged {
        node_id: NodeId,
        previous_state: Option<RuntimeState>,
        state: RuntimeState,
    },
    EdgeTabStateChanged {
        node_id: NodeId,
        previous_state: Option<RuntimeState>,
        state: RuntimeState,
        reconnected: bool,
    },
    FolderFilesChanged {
        node_id: NodeId,
        files: Vec<PathBuf>,
    },
    ReplayStateChanged {
        node_id: NodeId,
        snapshot: ReplayStateSnapshot,
    },
    ReplayPositionChanged {
        node_id: NodeId,
        position_seconds: f64,
        duration_seconds: Option<f64>,
    },
    RecordingStateChanged {
        node_id: NodeId,
        snapshot: RecorderStateSnapshot,
    },
    RecordingDurationChanged {
        node_id: NodeId,
        duration_seconds: f64,
    },
    NodeRuntimeStateChanged {
        node_id: NodeId,
        previous_state: Option<RuntimeState>,
        state: RuntimeState,
    },
    AudioLevelChanged {
        node_id: NodeId,
        port_id: Option<PortId>,
        meter: MeterFrame,
    },
    AudioClipping {
        node_id: NodeId,
        peak: f32,
    },
    RoutingChanged {
        revision: u64,
        edges: Vec<Edge>,
    },
    LoopWarningChanged {
        warnings: Vec<LoopWarning>,
    },
    LatencyChanged {
        node_id: NodeId,
        stage: crate::audio::clock::LatencyStage,
        snapshot: crate::audio::clock::LatencySnapshot,
        accuracy: crate::audio::clock::LatencyAccuracy,
    },
    PresetApplied {
        preset: crate::config::model::LatencyPreset,
        values: crate::config::model::PresetValues,
    },
    PresetApplyFailed {
        failed_node: NodeId,
        reason: String,
        rolled_back: bool,
    },
    PluginStatsChanged {
        node_id: NodeId,
        stats: PluginStats,
        inference_warning: bool,
    },
    NodeRestarting {
        node_id: NodeId,
        reason: NodeRestartReason,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeRestartReason {
    PresetApply,
}

impl BackendEvent {
    pub const EVENT_NAMES: [&'static str; 20] = [
        "device-added",
        "device-state-changed",
        "device-removed",
        "process-state-changed",
        "edge-tab-state-changed",
        "folder-files-changed",
        "replay-state-changed",
        "replay-position-changed",
        "recording-state-changed",
        "recording-duration-changed",
        "node-runtime-state-changed",
        "audio-level-changed",
        "audio-clipping",
        "routing-changed",
        "loop-warning-changed",
        "latency-changed",
        "preset-applied",
        "preset-apply-failed",
        "plugin-stats-changed",
        "node-restarting",
    ];

    pub fn event_name(&self) -> &'static str {
        match self {
            Self::DeviceAdded { .. } => "device-added",
            Self::DeviceStateChanged { .. } => "device-state-changed",
            Self::DeviceRemoved { .. } => "device-removed",
            Self::ProcessStateChanged { .. } => "process-state-changed",
            Self::EdgeTabStateChanged { .. } => "edge-tab-state-changed",
            Self::FolderFilesChanged { .. } => "folder-files-changed",
            Self::ReplayStateChanged { .. } => "replay-state-changed",
            Self::ReplayPositionChanged { .. } => "replay-position-changed",
            Self::RecordingStateChanged { .. } => "recording-state-changed",
            Self::RecordingDurationChanged { .. } => "recording-duration-changed",
            Self::NodeRuntimeStateChanged { .. } => "node-runtime-state-changed",
            Self::AudioLevelChanged { .. } => "audio-level-changed",
            Self::AudioClipping { .. } => "audio-clipping",
            Self::RoutingChanged { .. } => "routing-changed",
            Self::LoopWarningChanged { .. } => "loop-warning-changed",
            Self::LatencyChanged { .. } => "latency-changed",
            Self::PresetApplied { .. } => "preset-applied",
            Self::PresetApplyFailed { .. } => "preset-apply-failed",
            Self::PluginStatsChanged { .. } => "plugin-stats-changed",
            Self::NodeRestarting { .. } => "node-restarting",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceFingerprint {
    friendly_name: String,
    connection_state: DeviceConnectionState,
    capture_enabled: bool,
    monitor_enabled: bool,
}

impl From<&KnownDevice> for DeviceFingerprint {
    fn from(device: &KnownDevice) -> Self {
        Self {
            friendly_name: device.friendly_name.clone(),
            connection_state: device.connection_state,
            capture_enabled: device.capture_enabled,
            monitor_enabled: device.monitor_enabled,
        }
    }
}

#[derive(Default)]
pub struct BackendEventMonitor {
    primed: bool,
    devices: HashMap<AppDeviceId, (KnownDevice, DeviceFingerprint)>,
    node_states: HashMap<NodeId, (NodeType, RuntimeState)>,
    replay_states: HashMap<NodeId, ReplayStateSnapshot>,
    recorder_states: HashMap<NodeId, RecorderStateSnapshot>,
    folder_files: HashMap<NodeId, Vec<PathBuf>>,
    edges: Vec<Edge>,
    loop_warnings: Vec<LoopWarning>,
    plugin_stats_emitted_at: HashMap<NodeId, Instant>,
}

impl BackendEventMonitor {
    pub fn poll(&mut self, backend: &Backend) -> Vec<BackendEvent> {
        let graph = backend.controller.snapshot();
        let devices: HashMap<_, _> = backend
            .device_history
            .read()
            .devices
            .iter()
            .map(|(id, device)| (*id, (device.clone(), DeviceFingerprint::from(device))))
            .collect();
        let node_states: HashMap<_, _> = graph
            .nodes
            .values()
            .map(|node| (node.id, (node.node_type, node.state)))
            .collect();
        let mut replay_states = HashMap::new();
        let mut recorder_states = HashMap::new();
        let mut folder_files = HashMap::new();
        for node in graph.nodes.values() {
            if matches!(
                node.node_type,
                NodeType::FileInput | NodeType::FolderReplay | NodeType::MemoryReplay
            ) {
                if let Ok(snapshot) = backend.nodes.replay_state(node.id) {
                    replay_states.insert(node.id, snapshot);
                }
            }
            if matches!(node.node_type, NodeType::Recorder | NodeType::ReplayCache) {
                if let Ok(snapshot) = backend.nodes.recorder_state(node.id) {
                    recorder_states.insert(node.id, snapshot);
                }
            }
            if node.node_type == NodeType::FolderReplay {
                if let Ok(files) = backend.nodes.cached_folder_files(node.id) {
                    folder_files.insert(node.id, files);
                }
            }
        }
        let loop_warnings = detect_loop_warnings(&graph);

        if !self.primed {
            self.primed = true;
            self.devices = devices;
            self.node_states = node_states;
            self.replay_states = replay_states;
            self.recorder_states = recorder_states;
            self.folder_files = folder_files;
            self.edges = graph.edges;
            self.loop_warnings = loop_warnings;
            return Vec::new();
        }

        let mut events = backend.take_pending_events();
        for (id, (device, fingerprint)) in &devices {
            match self.devices.get(id) {
                None => events.push(BackendEvent::DeviceAdded {
                    device: device.clone(),
                }),
                Some((previous, previous_fingerprint)) if previous_fingerprint != fingerprint => {
                    events.push(BackendEvent::DeviceStateChanged {
                        device: device.clone(),
                        previous_state: Some(previous.connection_state),
                    });
                }
                _ => {}
            }
        }
        for id in self.devices.keys().filter(|id| !devices.contains_key(id)) {
            events.push(BackendEvent::DeviceRemoved { app_device_id: *id });
        }

        for (node_id, (node_type, state)) in &node_states {
            let previous = self.node_states.get(node_id).map(|(_, state)| *state);
            if previous != Some(*state) {
                events.push(BackendEvent::NodeRuntimeStateChanged {
                    node_id: *node_id,
                    previous_state: previous,
                    state: *state,
                });
                if matches!(node_type, NodeType::ProcessPid | NodeType::ProcessExe) {
                    events.push(BackendEvent::ProcessStateChanged {
                        node_id: *node_id,
                        previous_state: previous,
                        state: *state,
                    });
                }
                if *node_type == NodeType::EdgeTab {
                    events.push(BackendEvent::EdgeTabStateChanged {
                        node_id: *node_id,
                        previous_state: previous,
                        state: *state,
                        reconnected: *state == RuntimeState::Running
                            && previous.is_some_and(|previous| {
                                matches!(
                                    previous,
                                    RuntimeState::Disconnected | RuntimeState::Waiting
                                )
                            }),
                    });
                }
            }
        }

        for (node_id, snapshot) in &replay_states {
            match self.replay_states.get(node_id) {
                Some(previous) => {
                    if replay_state_key(previous) != replay_state_key(snapshot) {
                        events.push(BackendEvent::ReplayStateChanged {
                            node_id: *node_id,
                            snapshot: snapshot.clone(),
                        });
                    }
                    if (previous.position_seconds - snapshot.position_seconds).abs() >= 0.015 {
                        events.push(BackendEvent::ReplayPositionChanged {
                            node_id: *node_id,
                            position_seconds: snapshot.position_seconds,
                            duration_seconds: snapshot.duration_seconds,
                        });
                    }
                }
                None => events.push(BackendEvent::ReplayStateChanged {
                    node_id: *node_id,
                    snapshot: snapshot.clone(),
                }),
            }
        }

        for (node_id, snapshot) in &recorder_states {
            match self.recorder_states.get(node_id) {
                Some(previous) => {
                    if recorder_state_key(previous) != recorder_state_key(snapshot) {
                        events.push(BackendEvent::RecordingStateChanged {
                            node_id: *node_id,
                            snapshot: snapshot.clone(),
                        });
                    }
                    if (previous.duration_seconds - snapshot.duration_seconds).abs() >= 0.015 {
                        events.push(BackendEvent::RecordingDurationChanged {
                            node_id: *node_id,
                            duration_seconds: snapshot.duration_seconds,
                        });
                    }
                }
                None => events.push(BackendEvent::RecordingStateChanged {
                    node_id: *node_id,
                    snapshot: snapshot.clone(),
                }),
            }
        }

        for (node_id, files) in &folder_files {
            if self.folder_files.get(node_id) != Some(files) {
                events.push(BackendEvent::FolderFilesChanged {
                    node_id: *node_id,
                    files: files.clone(),
                });
            }
        }
        if self.edges != graph.edges {
            events.push(BackendEvent::RoutingChanged {
                revision: graph.revision,
                edges: graph.edges.clone(),
            });
        }
        if self.loop_warnings != loop_warnings {
            events.push(BackendEvent::LoopWarningChanged {
                warnings: loop_warnings.clone(),
            });
        }

        let now = Instant::now();
        let block_frames = backend.runtime.settings().block_frames;
        let block_ms = frames_to_ms(block_frames, INTERNAL_SAMPLE_RATE);
        for node in graph.nodes.values() {
            let NodeProperties::TransformPlugin { instance_id, .. } = &node.properties else {
                continue;
            };
            let Ok(processor) = backend.plugins.get(*instance_id) else {
                continue;
            };
            processor.refresh_stats();
            let Some(stats) = processor.cached_stats() else {
                continue;
            };
            let due = self
                .plugin_stats_emitted_at
                .get(&node.id)
                .map(|previous| now.saturating_duration_since(*previous) >= Duration::from_secs(1))
                .unwrap_or(true);
            if !due {
                continue;
            }
            let inference_warning = stats
                .inference_ms
                .is_some_and(|inference_ms| inference_ms > block_ms);
            events.push(BackendEvent::PluginStatsChanged {
                node_id: node.id,
                stats,
                inference_warning,
            });
            self.plugin_stats_emitted_at.insert(node.id, now);
        }

        self.devices = devices;
        self.node_states = node_states;
        self.replay_states = replay_states;
        self.recorder_states = recorder_states;
        self.folder_files = folder_files;
        self.edges = graph.edges;
        self.loop_warnings = loop_warnings;
        events
    }
}

fn replay_state_key(
    snapshot: &ReplayStateSnapshot,
) -> (ReplayPlaybackState, Option<u64>, bool, Option<&str>) {
    (
        snapshot.state,
        snapshot.duration_seconds.map(f64::to_bits),
        snapshot.eof,
        snapshot.current_file.as_deref(),
    )
}

fn recorder_state_key(
    snapshot: &RecorderStateSnapshot,
) -> (
    RecorderState,
    bool,
    bool,
    bool,
    bool,
    Option<&PathBuf>,
    Option<&str>,
) {
    (
        snapshot.state,
        snapshot.auto_record,
        snapshot.session_active,
        snapshot.input_available,
        snapshot.manual_stop_override,
        snapshot.current_path.as_ref(),
        snapshot.error.as_deref(),
    )
}
