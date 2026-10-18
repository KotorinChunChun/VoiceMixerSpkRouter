//! Backend command facade. The functions in this module intentionally do not
//! depend on Tauri types, so they are directly testable. A Tauri application
//! can wrap them with `#[tauri::command]` and managed `Arc<Backend>` state.

#[path = "workflow.rs"]
mod workflow;

pub fn workflow_action(
    backend: &Backend,
    action: String,
    payload: serde_json::Value,
) -> Result<serde_json::Value> {
    workflow::dispatch(backend, action, payload)
}

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub use crate::profile::test_configuration::TestConfigurationProfileReport;
pub use crate::windows::microphone_recording::{
    MicrophoneRecordingReport, MicrophoneRecordingRequest, WavRecordingMetrics,
};

use crate::{
    actions::model::{
        ActionExecutionId, ActionNodeConfig, ActionRuntimeState, LogOutputConfig,
        ObsConnectionConfig, ObsConnectionId, SecretId,
    },
    audio::{
        clock::{frames_to_ms, PathLatencyReport},
        engine::RuntimeSettings,
        format::INTERNAL_SAMPLE_RATE,
        graph::{
            ApplicationFilter, ApplicationFilterMode, CacheId, CaptureMethod, ChannelMapperConfig,
            ConnectionEndpoint, DelayConfig, Edge, EdgeId, EndpointCaptureMode, EventEdge,
            FilterRuleId, FolderReplayMode, GraphSnapshot, GroupProcessorId, GroupProcessorSpec,
            InputSelectorConfig, MemoryReplayMode, NodeId, NodeLayout, NodeProperties, NodeSpec,
            NodeType, PluginInstanceId, PortId, ProcessFilterRule, ProcessFilterTarget,
            RecordingFormat, RegisteredNodeId, ReplayItemId, RuntimeState, RvcInstanceId,
            SilenceDetectorConfig, SwitchPolicy, TestSignalConfig,
        },
    },
    config::{
        model::{
            expand_preset, ApplicationSettings, LatencyPreset, PersistedConfiguration, PresetValues,
        },
        routing_profile::{
            self, RoutingProfile, RoutingProfileSummary, ROUTING_PROFILE_SCHEMA_VERSION,
        },
    },
    edge::{
        control::EdgeMediaAction,
        receiver::{EdgeMediaResult, EdgeTabSummary},
    },
    error::{BackendError, Result},
    nodes::input::test_signal::TestSignalSource,
    plugins::{
        builtins::limiter::LIMITER_PLUGIN_ID,
        instance::{PluginPrepareContext, PluginState},
        manifest::{PluginDescriptor, PluginExecutionMode},
        package::PluginDiscoveryReport,
        process_protocol::PluginStats,
    },
    profile::volume_profile::{ApplyVolumeProfileReport, VolumeMuteProfile, VolumeProfileId},
    recording::{
        state::{RecorderState, RecorderStateSnapshot, RecordingOutput},
        trim::{export_trimmed, TrimRange},
        waveform::{get_cached_waveform, get_waveform, WaveformBucket},
    },
    registry::{
        device_history::{AppDeviceId, DeviceConnectionState, DeviceUsage, KnownDevice},
        instance_policy::InstancePolicy,
        node_registry::{RegisteredNode, RegistrationOrigin},
    },
    replay::{
        cache::{
            migrate_legacy_replay_cache_name, ReplayCacheConfig, ReplayCacheItem,
            ReplayCacheMetadata,
        },
        folder_player::FolderReplayPlayer,
        player::{MemoryReplayPlayer, ReplaySource},
        state::ReplayStateSnapshot,
    },
    routing::{
        loop_detector::LoopWarning,
        traversal::{
            downstream_nodes, related_route_nodes, route_to_output, upstream_nodes, RouteSubgraph,
        },
        validator::{ConnectionCandidateResult, CyclePolicy, ValidationReport},
    },
    runtime_manager::NodeStartResult,
    rvc::adapter::{PassthroughRvcBackend, RvcNodeProcessor, RvcState},
    shortcut::{
        dispatcher::NodeActionDispatcher,
        model::{
            default_reserved_combinations, KeyCombination, NodeActionDescriptor, ShortcutBinding,
            ShortcutId, ShortcutRegistrationState, ShortcutValidation,
        },
    },
    windows::{
        devices::{
            default_device_endpoints, list_devices, read_endpoint_peaks, resolve_endpoint_id,
            set_default_audio_device, AudioDeviceInfo, DefaultDeviceEndpoints, DeviceDirection,
        },
        implicit_routes::{
            detect_listen_to_device, list_implicit_routes, ImplicitRoute, ListenDetection,
            VirtualDevicePair,
        },
        input_monitor::{
            ensure_distinct_monitor_endpoints, unsupported_input_monitor_type, InputMonitorScope,
            InputMonitorSession, InputMonitorSnapshot, InputMonitorStatesSnapshot, PreviewSource,
        },
        meter_monitor::{direction_for_device_node, MeterMonitorTarget},
        process_watcher::{
            list_audio_processes, list_processes, FilteredCaptureStatus, ProcessInfo,
        },
        worker_status::DeviceWorkerSnapshot,
    },
    Backend,
};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeviceLists {
    pub inputs: Vec<AudioDeviceInfo>,
    pub outputs: Vec<AudioDeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordingInfo {
    pub node_id: NodeId,
    pub path: PathBuf,
    pub exists: bool,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InputEndpointLevel {
    pub registered_node_id: RegisteredNodeId,
    pub endpoint_id: String,
    pub peak: f32,
    #[serde(default)]
    pub rms: f32,
    pub peak_dbfs: f32,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub active_method: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateShortcutRequest {
    pub enabled: bool,
    pub key_combo: KeyCombination,
    pub node_id: NodeId,
    pub action_id: String,
    #[serde(default)]
    pub action_parameter: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FrontendSnapshot {
    pub registered_nodes: Vec<RegisteredNode>,
    pub graph: GraphSnapshot,
    pub known_devices: Vec<KnownDevice>,
    pub replay_caches: Vec<ReplayCacheConfig>,
    pub volume_profiles: Vec<VolumeMuteProfile>,
    pub replay_states: HashMap<NodeId, ReplayStateSnapshot>,
    pub recorder_states: HashMap<NodeId, RecorderStateSnapshot>,
    pub recordings: Vec<RecordingInfo>,
    pub transform_plugins: Vec<PluginDescriptor>,
    pub plugin_discovery: PluginDiscoveryReport,
    pub routing_profiles: Vec<RoutingProfileSummary>,
    pub routing_profile_directory: PathBuf,
    pub active_routing_profile: Option<String>,
    pub shortcuts: Vec<ShortcutBinding>,
    pub shortcut_registration_states: HashMap<ShortcutId, ShortcutRegistrationState>,
    pub device_worker_states: HashMap<NodeId, DeviceWorkerSnapshot>,
    pub default_devices: DefaultDeviceEndpoints,
    #[serde(default)]
    pub plugin_stats: HashMap<NodeId, PluginStatsReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct PluginStatsReport {
    pub stats: PluginStats,
    pub inference_warning: bool,
    pub switching: bool,
    pub switch_policy: SwitchPolicy,
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub fn frontend_snapshot(backend: &Backend) -> Result<FrontendSnapshot> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let mut replay_states = HashMap::new();
    let mut recorder_states = HashMap::new();
    let mut device_worker_states = HashMap::new();
    for node in graph.nodes.values() {
        if let Some(status) = backend.nodes.device_worker_status(node.id) {
            device_worker_states.insert(node.id, status);
        }
        match node.node_type {
            NodeType::FileInput | NodeType::FolderReplay | NodeType::MemoryReplay => {
                if let Ok(state) = backend.nodes.replay_state(node.id) {
                    replay_states.insert(node.id, state);
                }
            }
            NodeType::Recorder | NodeType::ReplayCache => {
                if let Ok(state) = backend.nodes.recorder_state(node.id) {
                    recorder_states.insert(node.id, state);
                }
            }
            _ => {}
        }
    }
    let shortcuts = backend.shortcut_engine.list();
    let shortcut_registration_states = shortcuts
        .iter()
        .map(|binding| {
            (
                binding.id,
                backend.shortcut_engine.registration_state(binding.id),
            )
        })
        .collect();
    let plugin_stats = collect_plugin_stats(backend, &graph);
    Ok(FrontendSnapshot {
        registered_nodes: backend.registry.list(),
        graph,
        known_devices: list_known_devices(backend),
        default_devices: default_device_endpoints(),
        replay_caches: backend.replay_caches.list_caches(),
        volume_profiles: list_volume_profiles(backend)?,
        replay_states,
        recorder_states,
        recordings: list_recordings(backend),
        transform_plugins: backend.plugins.list_plugins(),
        plugin_discovery: backend.plugin_discovery_report(),
        routing_profiles: routing_profile::list(&backend.config)?,
        routing_profile_directory: routing_profile::directory(&backend.config),
        active_routing_profile: backend.active_routing_profile.lock().clone(),
        shortcuts,
        shortcut_registration_states,
        device_worker_states,
        plugin_stats,
    })
}

fn collect_plugin_stats(
    backend: &Backend,
    graph: &GraphSnapshot,
) -> HashMap<NodeId, PluginStatsReport> {
    let mut reports = HashMap::new();
    for node in graph.nodes.values() {
        if let Ok(report) = plugin_stats_report(backend, node) {
            reports.insert(node.id, report);
        }
    }
    reports
}

fn plugin_stats_report(backend: &Backend, node: &NodeSpec) -> Result<PluginStatsReport> {
    let NodeProperties::TransformPlugin {
        instance_id,
        switch_policy,
        ..
    } = &node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not a TransformPlugin".into(),
        ));
    };
    let processor = backend.plugins.get(*instance_id)?;
    processor.refresh_stats();
    let stats = processor.cached_stats().unwrap_or_default();
    let block_ms = frames_to_ms(
        backend.runtime.settings().block_frames,
        INTERNAL_SAMPLE_RATE,
    );
    Ok(PluginStatsReport {
        stats: stats.clone(),
        inference_warning: stats
            .inference_ms
            .is_some_and(|inference_ms| inference_ms > block_ms),
        switching: processor.is_switching(),
        switch_policy: *switch_policy,
    })
}

pub fn get_plugin_stats(backend: &Backend, node_id: NodeId) -> Result<PluginStatsReport> {
    let _operation = backend.operation_lock.lock();
    let node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    plugin_stats_report(backend, &node)
}

pub fn open_windows_sound_settings() -> Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg("ms-settings:sound")
            .spawn()
            .map_err(BackendError::Io)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(BackendError::PlatformUnsupported(
            "Windowsサウンド設定はWindowsでのみ開けます".into(),
        ))
    }
}

pub fn input_endpoint_levels(backend: &Backend) -> Vec<InputEndpointLevel> {
    let _operation = backend.operation_lock.lock();
    let definitions: Vec<_> = backend
        .registry
        .list()
        .into_iter()
        .filter_map(|definition| match &definition.properties {
            NodeProperties::Microphone { endpoint_id, .. } => Some((
                definition.id,
                endpoint_id.clone(),
                resolve_endpoint_id(endpoint_id, DeviceDirection::Capture),
            )),
            NodeProperties::EndpointLoopback { endpoint_id, .. } => Some((
                definition.id,
                endpoint_id.clone(),
                resolve_endpoint_id(endpoint_id, DeviceDirection::Render),
            )),
            _ => None,
        })
        .collect();
    let resolved: Vec<_> = definitions
        .iter()
        .filter_map(|(_, _, endpoint)| endpoint.as_ref().ok().cloned())
        .collect();
    let readings = read_endpoint_peaks(&resolved);
    definitions
        .into_iter()
        .map(
            |(registered_node_id, configured_endpoint_id, endpoint)| match endpoint {
                Ok(endpoint_id) => {
                    let reading = readings
                        .iter()
                        .find(|reading| reading.endpoint_id == endpoint_id);
                    let peak = reading.map(|reading| reading.peak).unwrap_or(0.0);
                    InputEndpointLevel {
                        registered_node_id,
                        endpoint_id,
                        peak,
                        rms: 0.0,
                        peak_dbfs: if peak > 0.000_001 {
                            20.0 * peak.log10()
                        } else {
                            -120.0
                        },
                        state: "legacy_endpoint_meter".into(),
                        active_method: None,
                        error: reading
                            .map(|reading| reading.error.clone())
                            .unwrap_or_else(|| {
                                Some("endpoint meter result was not returned".into())
                            }),
                        error_kind: None,
                    }
                }
                Err(error) => InputEndpointLevel {
                    registered_node_id,
                    endpoint_id: configured_endpoint_id,
                    peak: 0.0,
                    rms: 0.0,
                    peak_dbfs: -120.0,
                    state: "error".into(),
                    active_method: None,
                    error: Some(error.to_string()),
                    error_kind: None,
                },
            },
        )
        .collect()
}

pub fn set_meter_monitor_targets(
    backend: &Backend,
    registered_node_ids: Vec<RegisteredNodeId>,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let disconnected_endpoints = backend
        .device_history
        .read()
        .devices
        .values()
        .filter(|device| device.connection_state == DeviceConnectionState::Disconnected)
        .map(|device| (device.endpoint_id.clone(), device.direction))
        .collect::<Vec<_>>();
    let targets = registered_node_ids
        .into_iter()
        .filter_map(|registered_node_id| {
            let definition = backend.registry.get(registered_node_id).ok()?;
            let direction = direction_for_device_node(definition.node_type)?;
            let (endpoint_id, device_direction) = match &definition.properties {
                NodeProperties::Microphone { endpoint_id, .. } => {
                    (endpoint_id, DeviceDirection::Capture)
                }
                NodeProperties::EndpointLoopback { endpoint_id, .. }
                | NodeProperties::Monitor { endpoint_id, .. }
                | NodeProperties::VirtualDevice { endpoint_id, .. } => {
                    (endpoint_id, DeviceDirection::Render)
                }
                _ => return None,
            };
            let endpoint_id = resolve_endpoint_id(endpoint_id, device_direction).ok()?;
            if disconnected_endpoints
                .iter()
                .any(|(known_endpoint, known_direction)| {
                    known_endpoint == &endpoint_id && *known_direction == device_direction
                })
            {
                return None;
            }
            Some(MeterMonitorTarget {
                registered_node_id,
                endpoint_id,
                direction,
            })
        })
        .collect();
    backend.meter_monitor.set_targets(targets)
}

pub fn meter_monitor_levels(backend: &Backend) -> Vec<InputEndpointLevel> {
    let _operation = backend.operation_lock.lock();
    backend
        .meter_monitor
        .readings()
        .into_iter()
        .map(|reading| InputEndpointLevel {
            registered_node_id: reading.registered_node_id,
            endpoint_id: reading.endpoint_id,
            peak: reading.frame.peak,
            rms: reading.frame.rms,
            peak_dbfs: if reading.frame.peak > 0.000_001 {
                20.0 * reading.frame.peak.log10()
            } else {
                -120.0
            },
            state: reading.state,
            active_method: reading.active_method,
            error: reading.error,
            error_kind: reading.error_kind,
        })
        .collect()
}

fn ensure_no_graph_audition(backend: &Backend) -> Result<()> {
    if backend.runtime.audition_target().is_some() {
        return Err(BackendError::InvalidOperation(
            "クイックモニタリング中です。解除してから通常出力・録音・入力試聴を操作してください"
                .into(),
        ));
    }
    Ok(())
}

fn ensure_output_operation_allowed(backend: &Backend, node_id: NodeId) -> Result<()> {
    if backend
        .runtime
        .audition_target()
        .is_none_or(|target| target.continue_outputs)
    {
        return Ok(());
    }
    if backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .is_some_and(|node| node.node_type.class() == crate::audio::graph::NodeClass::Output)
    {
        ensure_no_graph_audition(backend)?;
    }
    Ok(())
}

pub fn start_input_monitor(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    output_endpoint_id: Option<String>,
    scope: InputMonitorScope,
) -> Result<InputMonitorSnapshot> {
    let _operation = backend.operation_lock.lock();
    ensure_no_graph_audition(backend)?;
    let definition = backend.registry.get(registered_node_id)?;
    let monitor_block_frames = backend.config.load()?.settings.block_frames;
    let output_endpoint_id = resolve_endpoint_id(
        output_endpoint_id
            .as_deref()
            .unwrap_or(crate::windows::devices::DEFAULT_CONSOLE_ENDPOINT),
        DeviceDirection::Render,
    )?;
    let session = match &definition.properties {
        NodeProperties::Microphone {
            endpoint_id,
            capture_method,
            ..
        } => {
            let input_endpoint_id = resolve_endpoint_id(endpoint_id, DeviceDirection::Capture)?;
            let (previous, token) = backend
                .input_monitor
                .begin_start(registered_node_id, scope)?;
            drop(previous);
            let session = InputMonitorSession::start_microphone(
                registered_node_id,
                scope,
                input_endpoint_id,
                *capture_method,
                output_endpoint_id,
                monitor_block_frames,
            );
            (session, token)
        }
        NodeProperties::EndpointLoopback { endpoint_id, .. } => {
            let input_endpoint_id = resolve_endpoint_id(endpoint_id, DeviceDirection::Render)?;
            ensure_distinct_monitor_endpoints(&input_endpoint_id, &output_endpoint_id)?;
            let (previous, token) = backend
                .input_monitor
                .begin_start(registered_node_id, scope)?;
            drop(previous);
            let session = InputMonitorSession::start_loopback(
                registered_node_id,
                scope,
                input_endpoint_id,
                output_endpoint_id,
                monitor_block_frames,
            );
            (session, token)
        }
        _ => return Err(unsupported_input_monitor_type()),
    };
    let (session, token) = session;
    match session {
        Ok(session) => backend
            .input_monitor
            .finish_start(registered_node_id, token, session),
        Err(error) => {
            backend.input_monitor.cancel_start(registered_node_id);
            Err(error)
        }
    }
}

pub fn start_input_preview(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    output_endpoint_id: Option<String>,
    scope: InputMonitorScope,
) -> Result<InputMonitorSnapshot> {
    let _operation = backend.operation_lock.lock();
    ensure_no_graph_audition(backend)?;
    let definition = backend.registry.get(registered_node_id)?;
    let monitor_block_frames = backend.config.load()?.settings.block_frames;
    let output_endpoint_id = resolve_endpoint_id(
        output_endpoint_id
            .as_deref()
            .unwrap_or(crate::windows::devices::DEFAULT_CONSOLE_ENDPOINT),
        DeviceDirection::Render,
    )?;
    let (input_label, source): (String, PreviewSource) = match &definition.properties {
        NodeProperties::FileInput {
            path,
            looped,
            loop_wait_ms,
            ..
        } => {
            let source = std::sync::Arc::new(MemoryReplayPlayer::open_with_wait(
                path,
                *looped,
                *loop_wait_ms,
            )?);
            source.play()?;
            (path.display().to_string(), PreviewSource::Replay(source))
        }
        NodeProperties::FolderReplay {
            path,
            mode,
            selected_file,
            ..
        } => {
            let source = std::sync::Arc::new(FolderReplayPlayer::open(
                path,
                *mode,
                selected_file.clone(),
            )?);
            source.play()?;
            (path.display().to_string(), PreviewSource::Replay(source))
        }
        NodeProperties::MemoryReplay {
            cache_id,
            mode,
            selected_item_id,
            ..
        } => {
            let item = match (mode, selected_item_id) {
                (MemoryReplayMode::SelectFromCache, Some(item_id)) => {
                    backend.replay_caches.get_item(*cache_id, *item_id)?
                }
                _ => backend.replay_caches.latest(*cache_id)?.ok_or_else(|| {
                    BackendError::InvalidOperation("再生できるメモリー項目がありません".into())
                })?,
            };
            let source = std::sync::Arc::new(MemoryReplayPlayer::open(&item.path, false)?);
            source.play()?;
            (
                item.path.display().to_string(),
                PreviewSource::Replay(source),
            )
        }
        NodeProperties::TestSignal { config, .. } => (
            "test-signal".into(),
            PreviewSource::Audio(Box::new(TestSignalSource::new(*config))),
        ),
        _ => return Err(unsupported_input_monitor_type()),
    };
    let (previous, token) = backend
        .input_monitor
        .begin_start(registered_node_id, scope)?;
    drop(previous);
    let session = InputMonitorSession::start_preview(
        registered_node_id,
        scope,
        input_label,
        source,
        output_endpoint_id,
        monitor_block_frames,
    );
    match session {
        Ok(session) => backend
            .input_monitor
            .finish_start(registered_node_id, token, session),
        Err(error) => {
            backend.input_monitor.cancel_start(registered_node_id);
            Err(error)
        }
    }
}

pub fn stop_input_monitor(backend: &Backend) {
    let _operation = backend.operation_lock.lock();
    backend.input_monitor.stop_all();
}

pub fn stop_audition(backend: &Backend) {
    let _operation = backend.operation_lock.lock();
    // 試聴の再生待ちを先に終了し、通常配送を再開する。
    let had_session = backend.runtime.audition_target().is_some();
    let render = backend.audition_render.lock().take();
    let had_render = render.is_some();
    drop(render);
    backend.runtime.set_audition(None);
    if had_session || had_render {
        backend.nodes.refresh_recorder_inputs();
    }
}

pub fn audition_state(backend: &Backend) -> Option<crate::audio::audition::AuditionTarget> {
    let failed = backend
        .audition_render
        .lock()
        .as_ref()
        .is_some_and(|render| {
            render.status().state != crate::windows::worker_status::DeviceWorkerState::Running
        });
    if failed || backend.runtime.audition_target().is_none() {
        stop_audition(backend);
    }
    backend.runtime.audition_target()
}
pub fn audition_comparison_report(backend: &Backend) -> crate::audio::comparison::ComparisonReport {
    backend
        .runtime
        .audition_comparison()
        .map(|meter| meter.lock().report.clone())
        .unwrap_or_default()
}

pub fn start_audition(
    backend: &Backend,
    target: crate::audio::audition::AuditionTarget,
    endpoint_id: String,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    if !backend.input_monitor.states().sessions.is_empty() {
        return Err(BackendError::InvalidOperation(
            "既存の入力試聴を停止してからクイックモニタリングを開始してください".into(),
        ));
    }
    if !backend.runtime.is_running() {
        return Err(BackendError::InvalidOperation(
            "音声エンジンが停止しています。通常の開始操作を行ってから試聴してください".into(),
        ));
    }
    if !target.gain_db.is_finite()
        || target.gain_db > 0.0
        || target.gain_db < -120.0
        || (target.input_port.is_some() && target.output_port.is_some())
        || (target.match_rms && target.comparison_input.is_none())
    {
        return Err(BackendError::InvalidOperation(
            "試聴のPort・補正値が不正です".into(),
        ));
    }
    let graph = backend.controller.snapshot();
    let node = graph
        .nodes
        .get(&target.node_id)
        .ok_or_else(|| BackendError::NodeNotFound(target.node_id.to_string()))?;
    if !node.enabled
        || !node.runtime_enabled
        || matches!(node.state, RuntimeState::Stopped | RuntimeState::Error)
    {
        return Err(BackendError::InvalidOperation(
            "対象ノードはOFFまたは停止中です。ノードを開始してから試聴してください".into(),
        ));
    }
    if !node
        .input_ports
        .iter()
        .chain(&node.output_ports)
        .any(|port| port.signal_kind == crate::audio::graph::SignalKind::Audio)
    {
        return Err(BackendError::InvalidOperation(
            "このノードに音声はありません".into(),
        ));
    }
    if target.input_port.is_some_and(|id| {
        !node
            .input_ports
            .iter()
            .any(|port| port.id == id && port.signal_kind == crate::audio::graph::SignalKind::Audio)
    }) || target.output_port.is_some_and(|id| {
        !node
            .output_ports
            .iter()
            .any(|port| port.id == id && port.signal_kind == crate::audio::graph::SignalKind::Audio)
    }) || target.comparison_input.is_some_and(|id| {
        !node
            .input_ports
            .iter()
            .any(|port| port.id == id && port.signal_kind == crate::audio::graph::SignalKind::Audio)
    }) {
        return Err(BackendError::InvalidOperation(
            "試聴Portがありません".into(),
        ));
    }
    let ring = crate::audio::buffer::AudioRingBuffer::new_overwriting(48_000 / 5 * 2);
    let render = crate::windows::wasapi_render::start_render(endpoint_id, ring.clone())?;
    render.wait_until_running(std::time::Duration::from_secs(3))?;
    let comparison = if backend.runtime.audition_target().is_some_and(|previous| {
        previous.node_id == target.node_id && previous.comparison_input == target.comparison_input
    }) {
        backend.runtime.audition_comparison().unwrap_or_default()
    } else {
        Arc::default()
    };
    backend
        .runtime
        .set_audition(Some(crate::audio::audition::AuditionSession {
            target,
            sink: Arc::new(crate::nodes::output::monitor::RingOutputSink::new(ring)),
            comparison,
            failed: std::sync::atomic::AtomicBool::new(false),
        }));
    *backend.audition_render.lock() = Some(render);
    Ok(())
}

pub fn stop_input_monitor_for(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
) -> InputMonitorStatesSnapshot {
    let _operation = backend.operation_lock.lock();
    backend.input_monitor.stop_for(registered_node_id)
}

pub fn stop_input_monitors_for_scope(
    backend: &Backend,
    scope: InputMonitorScope,
) -> InputMonitorStatesSnapshot {
    let _operation = backend.operation_lock.lock();
    backend.input_monitor.stop_scope(scope)
}

pub fn play_input_preview(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
) -> Result<InputMonitorSnapshot> {
    let _operation = backend.operation_lock.lock();
    let (_, snapshot) = backend
        .input_monitor
        .with_session(registered_node_id, |session| {
            if !session.is_replay_preview_for(registered_node_id) {
                return Err(BackendError::InvalidOperation(
                    "指定された入力の再生プレビューが開始されていません".into(),
                ));
            }
            session.play_preview()
        })?;
    Ok(snapshot)
}

pub fn pause_input_preview(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
) -> Result<InputMonitorSnapshot> {
    let _operation = backend.operation_lock.lock();
    let (_, snapshot) = backend
        .input_monitor
        .with_session(registered_node_id, |session| {
            if !session.is_replay_preview_for(registered_node_id) {
                return Err(BackendError::InvalidOperation(
                    "指定された入力の再生プレビューが開始されていません".into(),
                ));
            }
            session.pause_preview()
        })?;
    Ok(snapshot)
}

pub fn seek_input_preview(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    seconds: f64,
) -> Result<InputMonitorSnapshot> {
    let _operation = backend.operation_lock.lock();
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(BackendError::InvalidOperation(
            "seek position must be a finite non-negative value".into(),
        ));
    }
    let (_, snapshot) = backend
        .input_monitor
        .with_session(registered_node_id, |session| {
            if !session.is_replay_preview_for(registered_node_id) {
                return Err(BackendError::InvalidOperation(
                    "指定された入力の再生プレビューが開始されていません".into(),
                ));
            }
            session.seek_preview(std::time::Duration::from_secs_f64(seconds))
        })?;
    Ok(snapshot)
}

pub fn input_monitor_state(backend: &Backend) -> Option<InputMonitorSnapshot> {
    let _operation = backend.operation_lock.lock();
    backend.input_monitor.legacy_state()
}

pub fn input_monitor_states(backend: &Backend) -> InputMonitorStatesSnapshot {
    let _operation = backend.operation_lock.lock();
    backend.input_monitor.states()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RvcConfiguration {
    pub instance_id: RvcInstanceId,
    /// Placeholder backend is useful for graph/integration tests. Replace it
    /// with the concrete RVC IPC/FFI backend when that still-open design item
    /// is decided.
    pub passthrough_latency_frames: u32,
}

pub fn list_input_devices(backend: &Backend) -> Result<Vec<AudioDeviceInfo>> {
    let _operation = backend.operation_lock.lock();
    apply_device_preferences(backend, list_devices(DeviceDirection::Capture)?)
}

pub fn list_output_devices(backend: &Backend) -> Result<Vec<AudioDeviceInfo>> {
    let _operation = backend.operation_lock.lock();
    apply_device_preferences(backend, list_devices(DeviceDirection::Render)?)
}

/// Records one exact capture Endpoint without involving the graph or UI.
pub fn record_microphone_wav(
    request: MicrophoneRecordingRequest,
) -> Result<MicrophoneRecordingReport> {
    crate::windows::microphone_recording::record_microphone_to_wav(&request)
}

pub fn refresh_devices(backend: &Backend) -> Result<DeviceLists> {
    let _operation = backend.operation_lock.lock();
    let devices = DeviceLists {
        inputs: list_input_devices(backend)?,
        outputs: list_output_devices(backend)?,
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    apply_device_refresh(backend, &devices, now)?;
    Ok(devices)
}

pub fn set_default_audio_device_command(
    backend: &Backend,
    endpoint_id: String,
    direction: DeviceDirection,
    communications: bool,
) -> Result<DeviceLists> {
    let _operation = backend.operation_lock.lock();
    set_default_audio_device(&endpoint_id, direction, communications)?;
    refresh_devices(backend)
}

/// Builds and persists the stopped-state VB-CABLE/RVC test graph using the
/// endpoints that are present on this Windows machine.
pub fn load_test_configuration_profile(
    backend: &Backend,
) -> Result<TestConfigurationProfileReport> {
    let _operation = backend.operation_lock.lock();
    backend.refresh_external_plugins()?;
    let devices = DeviceLists {
        inputs: list_input_devices(backend)?,
        outputs: list_output_devices(backend)?,
    };
    load_test_configuration_profile_with_devices(backend, &devices)
}

/// Deterministic variant used by integration tests. Device registration and
/// history receive the supplied snapshot before the profile graph is built.
pub fn load_test_configuration_profile_with_devices(
    backend: &Backend,
    devices: &DeviceLists,
) -> Result<TestConfigurationProfileReport> {
    let _operation = backend.operation_lock.lock();
    apply_device_refresh(backend, devices, unix_time_ms())?;
    crate::profile::test_configuration::load_test_configuration_profile_with_devices(
        backend,
        &devices.inputs,
        &devices.outputs,
    )
}

/// Applies an already enumerated device snapshot. Keeping enumeration outside
/// this function makes the complete discovery/registration lifecycle
/// deterministic in tests while the production command still uses WASAPI.
pub fn apply_device_refresh(
    backend: &Backend,
    devices: &DeviceLists,
    now_unix_ms: u64,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let previous_registry_len = backend.registry.list().len();
    let (capture_report, render_report) = {
        let mut history = backend.device_history.write();
        let capture =
            history.refresh_direction(DeviceDirection::Capture, &devices.inputs, now_unix_ms);
        let render =
            history.refresh_direction(DeviceDirection::Render, &devices.outputs, now_unix_ms);
        (capture, render)
    };
    ensure_device_registered_nodes(backend, &devices)?;
    let changed = !capture_report.added.is_empty()
        || !capture_report.reconnected.is_empty()
        || !capture_report.disconnected.is_empty()
        || !render_report.added.is_empty()
        || !render_report.reconnected.is_empty()
        || !render_report.disconnected.is_empty()
        || backend.registry.list().len() != previous_registry_len;
    if changed {
        save_current_configuration(backend)?;
    }
    Ok(())
}

pub fn list_known_devices(backend: &Backend) -> Vec<KnownDevice> {
    let _operation = backend.operation_lock.lock();
    let mut devices: Vec<_> = backend
        .device_history
        .read()
        .devices
        .values()
        .cloned()
        .collect();
    devices.sort_by(|a, b| {
        a.display_order
            .cmp(&b.display_order)
            .then_with(|| a.friendly_name.cmp(&b.friendly_name))
    });
    devices
}

pub fn set_known_device_usage(
    backend: &Backend,
    app_device_id: AppDeviceId,
    usage: DeviceUsage,
    enabled: bool,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let endpoint_id = {
        let mut history = backend.device_history.write();
        history.set_usage_enabled(app_device_id, usage, enabled)?;
        history
            .devices
            .get(&app_device_id)
            .map(|device| device.endpoint_id.clone())
            .ok_or_else(|| BackendError::InvalidOperation("known device disappeared".into()))?
    };
    for mut registered in backend.registry.list().into_iter().filter(|node| {
        node.resource_key.as_deref() == Some(endpoint_id.as_str())
            && match usage {
                DeviceUsage::Capture => matches!(
                    node.node_type,
                    NodeType::Microphone | NodeType::EndpointLoopback
                ),
                DeviceUsage::Monitor => {
                    matches!(node.node_type, NodeType::Monitor | NodeType::VirtualDevice)
                }
            }
    }) {
        registered.enabled = enabled;
        backend.registry.update(registered)?;
    }
    save_current_configuration(backend)
}

pub fn forget_device(backend: &Backend, app_device_id: AppDeviceId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let endpoint_id = backend
        .device_history
        .read()
        .devices
        .get(&app_device_id)
        .map(|device| device.endpoint_id.clone())
        .ok_or_else(|| BackendError::InvalidOperation("known device not found".into()))?;
    let definitions: Vec<_> = backend
        .registry
        .list()
        .into_iter()
        .filter(|node| node.resource_key.as_deref() == Some(endpoint_id.as_str()))
        .collect();
    let graph = backend.controller.snapshot();
    if definitions.iter().any(|definition| {
        graph
            .nodes
            .values()
            .any(|node| node.registered_node_id == Some(definition.id))
    }) {
        return Err(BackendError::InvalidOperation(
            "remove graph instances before forgetting the device".into(),
        ));
    }
    backend
        .device_history
        .write()
        .forget_disconnected(app_device_id)?;
    for definition in definitions {
        backend.registry.delete(definition.id)?;
    }
    save_current_configuration(backend)
}

fn ensure_device_registered_nodes(backend: &Backend, devices: &DeviceLists) -> Result<()> {
    let existing = backend.registry.list();
    for device in &devices.inputs {
        ensure_registered_device_node(
            backend,
            &existing,
            device,
            NodeProperties::Microphone {
                endpoint_id: device.endpoint_id.clone(),
                capture_method: crate::audio::graph::CaptureMethod::Auto,
                gain_db: 0.0,
                mute: false,
            },
            device.friendly_name.clone(),
        )?;
    }
    for device in &devices.outputs {
        ensure_registered_device_node(
            backend,
            &existing,
            device,
            NodeProperties::EndpointLoopback {
                endpoint_id: device.endpoint_id.clone(),
                filter: ApplicationFilter::default(),
                capture_mode: EndpointCaptureMode::EndpointLoopback,
                gain_db: 0.0,
                mute: false,
            },
            device.friendly_name.clone(),
        )?;
        ensure_registered_device_node(
            backend,
            &existing,
            device,
            NodeProperties::Monitor {
                endpoint_id: device.endpoint_id.clone(),
                gain_db: 0.0,
                mute: false,
            },
            device.friendly_name.clone(),
        )?;
    }
    Ok(())
}

fn ensure_registered_device_node(
    backend: &Backend,
    existing: &[RegisteredNode],
    device: &AudioDeviceInfo,
    properties: NodeProperties,
    display_name: String,
) -> Result<()> {
    let node_type = properties.node_type();
    let pairs = backend.config.load()?.virtual_device_pairs;
    let is_virtual = match node_type {
        NodeType::Microphone => pairs
            .iter()
            .any(|pair| pair.capture_endpoint_id == device.endpoint_id),
        NodeType::Monitor => pairs
            .iter()
            .any(|pair| pair.render_endpoint_id == device.endpoint_id),
        _ => false,
    };
    if let Some(mut registered) = existing
        .iter()
        .chain(backend.registry.list().iter())
        .find(|node| {
            node.node_type == node_type
                && node.resource_key.as_deref() == Some(device.endpoint_id.as_str())
        })
        .cloned()
    {
        let changed =
            registered.display_name != device.friendly_name || registered.is_virtual != is_virtual;
        if changed {
            registered.display_name = device.friendly_name.clone();
            registered.is_virtual = is_virtual;
            backend.registry.update(registered)?;
        }
        return Ok(());
    }
    let mut registered = RegisteredNode::user(display_name, properties);
    registered.origin = RegistrationOrigin::AutoDetected;
    registered.resource_key = Some(device.endpoint_id.clone());
    registered.is_virtual = is_virtual;
    backend.registry.register(registered)?;
    Ok(())
}

pub fn update_device_visibility(backend: &Backend, endpoint_id: &str, visible: bool) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let mut config = backend.config.load()?;
    config
        .settings
        .device_preferences
        .entry(endpoint_id.to_string())
        .or_default()
        .visible = visible;
    backend.config.save(&config)
}

pub fn update_device_order(backend: &Backend, endpoint_id: &str, order: i32) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let mut config = backend.config.load()?;
    config
        .settings
        .device_preferences
        .entry(endpoint_id.to_string())
        .or_default()
        .sort_order = order;
    backend.config.save(&config)
}

fn apply_device_preferences(
    backend: &Backend,
    mut devices: Vec<AudioDeviceInfo>,
) -> Result<Vec<AudioDeviceInfo>> {
    let config = backend.config.load()?;
    for device in &mut devices {
        if let Some(pref) = config.settings.device_preferences.get(&device.endpoint_id) {
            device.user_visible = pref.visible;
            if pref.sort_order != i32::MAX {
                device.user_sort_order = pref.sort_order;
            }
        }
    }
    devices.sort_by_key(|d| d.user_sort_order);
    Ok(devices)
}

pub fn list_processes_command() -> Vec<ProcessInfo> {
    list_processes()
}
pub fn list_audio_processes_command() -> Vec<ProcessInfo> {
    list_audio_processes()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProcessFilterTargetInfo {
    pub target: ProcessFilterTarget,
    pub label: String,
    pub audio_activity: Option<f32>,
}

/// Targets presented by the frontend filter editor. Windows System Sounds is a
/// first-class target rather than being disguised as an EXE/PID.
pub fn list_process_filter_targets() -> Vec<ProcessFilterTargetInfo> {
    let processes = list_audio_processes();
    let mut out = Vec::new();
    let mut seen_exe = std::collections::HashSet::<String>::new();
    for p in processes {
        out.push(ProcessFilterTargetInfo {
            target: ProcessFilterTarget::Pid(p.pid),
            label: format!("{} (PID {})", p.exe_name, p.pid),
            audio_activity: Some(p.audio_activity),
        });
        let normalized = p.exe_name.to_ascii_lowercase();
        if seen_exe.insert(normalized) {
            out.push(ProcessFilterTargetInfo {
                target: ProcessFilterTarget::ExeName(p.exe_name.clone()),
                label: format!("{} (all matching processes)", p.exe_name),
                audio_activity: Some(p.audio_activity),
            });
        }
    }
    out.push(ProcessFilterTargetInfo {
        target: ProcessFilterTarget::SystemSounds,
        label: "Windows System Sounds".into(),
        audio_activity: None,
    });
    out
}

pub fn get_settings(backend: &Backend) -> Result<ApplicationSettings> {
    let _operation = backend.operation_lock.lock();
    Ok(backend.config.load()?.settings)
}

pub fn update_settings(
    backend: &Backend,
    settings: ApplicationSettings,
) -> Result<ApplicationSettings> {
    let _operation = backend.operation_lock.lock();
    settings
        .validate()
        .map_err(BackendError::InvalidOperation)?;
    apply_io_and_runtime(backend, &settings)?;
    let mut config = backend.config.load()?;
    config.settings = settings.clone();
    backend.config.save(&config)?;
    let _ = backend.refresh_external_plugins()?;
    Ok(settings)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresetRejectReason {
    RecordingActive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApplyLatencyPresetResult {
    Applied {
        preset: LatencyPreset,
        values: PresetValues,
    },
    Rejected {
        reason: PresetRejectReason,
    },
    Failed {
        failed_node: NodeId,
        reason: String,
        rolled_back: bool,
    },
}

pub fn apply_latency_preset(
    backend: &Backend,
    preset: LatencyPreset,
) -> Result<ApplyLatencyPresetResult> {
    let _operation = backend.operation_lock.lock();
    if backend.nodes.any_recorder_recording() {
        return Ok(ApplyLatencyPresetResult::Rejected {
            reason: PresetRejectReason::RecordingActive,
        });
    }
    let mut config = backend.config.load()?;
    let previous = config.settings.clone();
    let values = expand_preset(preset, &config.settings);
    config.settings.apply_preset_values(preset, &values);
    config
        .settings
        .validate()
        .map_err(BackendError::InvalidOperation)?;
    apply_io_and_runtime(backend, &config.settings)?;
    backend.config.save(&config)?;
    backend.stop_graph_runtime();
    let results = backend.start_graph_runtime()?;
    if let Some(failed) = results
        .iter()
        .find(|result| result.state == RuntimeState::Error)
    {
        let mut rolled = backend.config.load()?;
        rolled.settings = previous.clone();
        apply_io_and_runtime(backend, &rolled.settings)?;
        backend.config.save(&rolled)?;
        backend.stop_graph_runtime();
        let _ = backend.start_graph_runtime()?;
        let result = ApplyLatencyPresetResult::Failed {
            failed_node: failed.node_id,
            reason: failed
                .message
                .clone()
                .unwrap_or_else(|| "node failed to start".into()),
            rolled_back: true,
        };
        backend.push_event(crate::events::BackendEvent::PresetApplyFailed {
            failed_node: failed.node_id,
            reason: failed
                .message
                .clone()
                .unwrap_or_else(|| "node failed to start".into()),
            rolled_back: true,
        });
        return Ok(result);
    }
    backend.push_event(crate::events::BackendEvent::PresetApplied {
        preset,
        values: values.clone(),
    });
    Ok(ApplyLatencyPresetResult::Applied { preset, values })
}

fn apply_io_and_runtime(backend: &Backend, settings: &ApplicationSettings) -> Result<()> {
    backend.runtime.update_settings(RuntimeSettings {
        block_frames: settings.block_frames,
        meter_fps: settings.meter_fps,
    })?;
    backend
        .nodes
        .set_latency_io(crate::runtime_manager::LatencyIoConfig {
            auto_capture_priority: settings.auto_capture_priority.clone(),
            render_exclusive: settings.render_exclusive,
        });
    backend
        .edge
        .set_jitter_target_fill(settings.jitter_target_fill);
    backend
        .edge
        .set_auth_token(settings.edge_auth_token.clone());
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TransformPluginCreateRequest {
    pub plugin_id: String,
    pub display_name: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, serde_json::Value>,
}

pub fn list_transform_plugins(backend: &Backend) -> Vec<PluginDescriptor> {
    let _operation = backend.operation_lock.lock();
    backend.plugins.list_plugins()
}

pub fn transform_plugin_discovery_report(backend: &Backend) -> PluginDiscoveryReport {
    let _operation = backend.operation_lock.lock();
    backend.plugin_discovery_report()
}

pub fn refresh_transform_plugins(backend: &Backend) -> Result<PluginDiscoveryReport> {
    let _operation = backend.operation_lock.lock();
    backend.refresh_external_plugins()
}

pub fn create_transform_plugin(
    backend: &Backend,
    mut request: TransformPluginCreateRequest,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    // Settings-file values override the built-in manifest defaults for newly
    // created limiter nodes; no limiter is inserted automatically.
    if request.plugin_id == LIMITER_PLUGIN_ID {
        let d = &backend.config.load()?.settings.limiter_defaults;
        request
            .parameters
            .entry("ceiling_db".into())
            .or_insert(serde_json::json!(d.ceiling_db));
        request
            .parameters
            .entry("lookahead_ms".into())
            .or_insert(serde_json::json!(d.lookahead_ms));
        request
            .parameters
            .entry("release_ms".into())
            .or_insert(serde_json::json!(d.release_ms));
    }
    let mut node = backend.plugins.build_node_spec(
        &request.plugin_id,
        request.display_name,
        request.parameters,
    )?;
    // Explicit user creation means the graph node is usable immediately.
    node.enabled = true;
    create_node(backend, node)
}

pub fn set_transform_plugin_parameter(
    backend: &Backend,
    node_id: NodeId,
    parameter_id: String,
    value: serde_json::Value,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::TransformPlugin {
        plugin_id,
        instance_id,
        parameters,
        ..
    } = &mut node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not a TransformPlugin".into(),
        ));
    };
    backend
        .plugins
        .validate_parameter(plugin_id, &parameter_id, &value)?;
    if backend.nodes.is_active(node_id) {
        backend
            .plugins
            .get(*instance_id)?
            .set_parameter(&parameter_id, value.clone())?;
    }
    parameters.insert(parameter_id, value);
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_transform_plugin_bypass(
    backend: &Backend,
    node_id: NodeId,
    bypass: bool,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::TransformPlugin {
        instance_id,
        bypass: stored,
        ..
    } = &mut node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not a TransformPlugin".into(),
        ));
    };
    *stored = bypass;
    if backend.nodes.is_active(node_id) {
        backend.plugins.get(*instance_id)?.set_bypass(bypass);
    }
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn restore_transform_settings(
    backend: &Backend,
    node_id: NodeId,
    parameters: BTreeMap<String, serde_json::Value>,
    bypass: bool,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let previous = graph
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::TransformPlugin {
        plugin_id,
        instance_id,
        parameters: old_parameters,
        bypass: old_bypass,
        ..
    } = &previous.properties
    else {
        return Err(BackendError::InvalidOperation(
            "音声プラグインではありません".into(),
        ));
    };
    for (id, value) in &parameters {
        backend.plugins.validate_parameter(plugin_id, id, value)?;
    }
    let mut complete = old_parameters.clone();
    complete.extend(parameters);
    let parameters = complete;
    let active = backend.nodes.is_active(node_id);
    let processor = if active {
        Some(backend.plugins.get(*instance_id)?)
    } else {
        None
    };
    let restart = backend
        .plugins
        .descriptor(plugin_id)?
        .parameters
        .iter()
        .any(|descriptor| {
            descriptor.restart_required
                && parameters.get(&descriptor.id) != old_parameters.get(&descriptor.id)
        });
    let mut next = previous.clone();
    if let NodeProperties::TransformPlugin {
        parameters: stored,
        bypass: stored_bypass,
        ..
    } = &mut next.properties
    {
        *stored = parameters.clone();
        *stored_bypass = bypass;
    }
    let mut candidate = graph.clone();
    candidate.nodes.insert(node_id, next.clone());
    if let Some(processor) = &processor {
        processor.set_parameters_batch(&parameters)?;
        processor.set_bypass(bypass);
    }
    let result =
        backend
            .controller
            .commit_graph_candidate(graph.revision, candidate, |candidate| {
                save_graph_configuration(backend, candidate.clone())
            });
    if let Err(error) = result {
        if let Some(processor) = &processor {
            processor.set_parameters_batch(old_parameters)?;
            processor.set_bypass(*old_bypass);
        }
        return Err(error);
    }
    if restart && active {
        backend.nodes.stop_node(node_id)?;
        backend.nodes.start_node(node_id)?;
    }
    Ok(next)
}

pub fn set_transform_plugin_switch_policy(
    backend: &Backend,
    node_id: NodeId,
    switch_policy: SwitchPolicy,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::TransformPlugin {
        instance_id,
        switch_policy: stored,
        ..
    } = &mut node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not a TransformPlugin".into(),
        ));
    };
    *stored = switch_policy;
    if backend.nodes.is_active(node_id) {
        backend
            .plugins
            .get(*instance_id)?
            .set_switch_policy(switch_policy);
    }
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

fn graph_node_and_definition(
    backend: &Backend,
    node_id: NodeId,
) -> Result<(NodeSpec, RegisteredNode)> {
    let node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let registered_node_id = node.registered_node_id.ok_or_else(|| {
        BackendError::InvalidOperation("node is not bound to a registered definition".into())
    })?;
    let definition = backend.registry.get(registered_node_id)?;
    Ok((node, definition))
}

pub fn save_node_preset(
    backend: &Backend,
    node_id: NodeId,
    name: String,
    overwrite: bool,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(BackendError::InvalidOperation(
            "preset name must not be empty".into(),
        ));
    }
    let (node, mut definition) = graph_node_and_definition(backend, node_id)?;
    let NodeProperties::TransformPlugin { parameters, .. } = &node.properties else {
        return Err(BackendError::InvalidOperation(
            "node presets are only available for TransformPlugin".into(),
        ));
    };
    if let Some(existing) = definition
        .presets
        .iter_mut()
        .find(|preset| preset.name == name)
    {
        if !overwrite {
            return Err(BackendError::PresetAlreadyExists(name));
        }
        existing.parameters = parameters.clone();
    } else {
        definition
            .presets
            .push(crate::registry::node_registry::NodePreset {
                name,
                parameters: parameters.clone(),
            });
    }
    let definition = backend.registry.update(definition)?;
    save_current_configuration(backend)?;
    Ok(definition)
}

pub fn apply_node_preset(
    backend: &Backend,
    node_id: NodeId,
    preset_name: String,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let (node, definition) = graph_node_and_definition(backend, node_id)?;
    let preset = definition
        .presets
        .iter()
        .find(|preset| preset.name == preset_name)
        .cloned()
        .ok_or_else(|| BackendError::PresetNotFound(preset_name))?;
    let mut updated = node.clone();
    let NodeProperties::TransformPlugin {
        plugin_id,
        instance_id,
        parameters,
        ..
    } = &mut updated.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node presets are only available for TransformPlugin".into(),
        ));
    };
    let descriptor = backend.plugins.descriptor(plugin_id)?;
    let previous_parameters = parameters.clone();
    for (parameter_id, value) in &preset.parameters {
        backend
            .plugins
            .validate_parameter(plugin_id, parameter_id, value)?;
        parameters.insert(parameter_id.clone(), value.clone());
    }
    let requires_restart = descriptor.parameters.iter().any(|parameter| {
        parameter.restart_required
            && previous_parameters.get(&parameter.id) != parameters.get(&parameter.id)
    });
    let plugin_id = plugin_id.clone();
    let instance_id = *instance_id;
    let next_parameters = parameters.clone();
    let active = backend.nodes.is_active(node_id);
    if active && requires_restart {
        backend.push_event(crate::events::BackendEvent::NodeRestarting {
            node_id,
            reason: crate::events::NodeRestartReason::PresetApply,
        });
        backend.push_event(crate::events::BackendEvent::NodeRuntimeStateChanged {
            node_id,
            previous_state: Some(RuntimeState::Running),
            state: RuntimeState::Recovering,
        });
        backend
            .nodes
            .set_node_state(node_id, RuntimeState::Recovering);
        let prepare = PluginPrepareContext {
            sample_rate: INTERNAL_SAMPLE_RATE,
            channels: 2,
            graph_block_frames: backend.runtime.settings().block_frames,
        };
        if let Err(error) =
            backend
                .plugins
                .recreate_instance(instance_id, &plugin_id, next_parameters, prepare)
        {
            backend.nodes.set_node_state(node_id, RuntimeState::Error);
            backend.push_event(crate::events::BackendEvent::NodeRuntimeStateChanged {
                node_id,
                previous_state: Some(RuntimeState::Recovering),
                state: RuntimeState::Error,
            });
            return Err(error);
        }
        backend.nodes.set_node_state(node_id, RuntimeState::Running);
        backend.push_event(crate::events::BackendEvent::NodeRuntimeStateChanged {
            node_id,
            previous_state: Some(RuntimeState::Recovering),
            state: RuntimeState::Running,
        });
        updated.state = RuntimeState::Running;
    } else if active {
        for (parameter_id, value) in &preset.parameters {
            backend
                .plugins
                .get(instance_id)?
                .set_parameter(parameter_id, value.clone())?;
        }
    }
    let updated = backend.controller.update_node(updated)?;
    if let Some(registered_node_id) = updated.registered_node_id {
        let mut bound = backend.registry.get(registered_node_id)?;
        bound.properties = updated.properties.clone();
        backend.registry.update(bound)?;
    }
    save_current_configuration(backend)?;
    Ok(updated)
}

pub fn transform_plugin_state(
    backend: &Backend,
    instance_id: PluginInstanceId,
) -> Result<PluginState> {
    let _operation = backend.operation_lock.lock();
    Ok(backend.plugins.get(instance_id)?.state())
}

pub fn set_application_filter_mode(
    backend: &Backend,
    node_id: NodeId,
    mode: ApplicationFilterMode,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::EndpointLoopback {
        filter,
        capture_mode,
        ..
    } = &mut node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not an EndpointLoopback input".into(),
        ));
    };
    filter.mode = mode;
    *capture_mode = if mode == ApplicationFilterMode::Disabled {
        crate::audio::graph::EndpointCaptureMode::EndpointLoopback
    } else {
        crate::audio::graph::EndpointCaptureMode::FilteredProcessComposite
    };
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    let out = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    if was_active {
        backend.nodes.start_node(node_id)?;
    }
    Ok(out)
}

pub fn add_process_filter_rule(
    backend: &Backend,
    node_id: NodeId,
    target: ProcessFilterTarget,
) -> Result<ProcessFilterRule> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::EndpointLoopback { filter, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not an EndpointLoopback input".into(),
        ));
    };
    let rule = ProcessFilterRule::new(target);
    filter.rules.push(rule.clone());
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    if was_active {
        backend.nodes.start_node(node_id)?;
    }
    Ok(rule)
}

pub fn update_process_filter_rule(
    backend: &Backend,
    node_id: NodeId,
    rule: ProcessFilterRule,
) -> Result<ProcessFilterRule> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::EndpointLoopback { filter, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not an EndpointLoopback input".into(),
        ));
    };
    let slot = filter
        .rules
        .iter_mut()
        .find(|x| x.id == rule.id)
        .ok_or_else(|| {
            BackendError::InvalidOperation(format!("filter rule {} not found", rule.id))
        })?;
    *slot = rule.clone();
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    if was_active {
        backend.nodes.start_node(node_id)?;
    }
    Ok(rule)
}

pub fn remove_process_filter_rule(
    backend: &Backend,
    node_id: NodeId,
    rule_id: FilterRuleId,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::EndpointLoopback { filter, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not an EndpointLoopback input".into(),
        ));
    };
    let before = filter.rules.len();
    filter.rules.retain(|x| x.id != rule_id);
    if filter.rules.len() == before {
        return Err(BackendError::InvalidOperation(format!(
            "filter rule {rule_id} not found"
        )));
    }
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    if was_active {
        backend.nodes.start_node(node_id)?;
    }
    Ok(())
}

pub fn get_filtered_capture_status(
    backend: &Backend,
    node_id: NodeId,
) -> Option<FilteredCaptureStatus> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.filtered_capture_status(node_id)
}

pub fn list_registered_nodes(backend: &Backend) -> Vec<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    backend.registry.list()
}

pub fn update_registered_node(
    backend: &Backend,
    mut node: RegisteredNode,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    if let NodeProperties::VirtualDevice {
        endpoint_id,
        gain_db,
        mute,
    } = node.properties.clone()
    {
        node.node_type = NodeType::Monitor;
        node.is_virtual = true;
        node.properties = NodeProperties::Monitor {
            endpoint_id,
            gain_db,
            mute,
        };
    }
    let node = backend.registry.update(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_input_capture_method(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    capture_method: CaptureMethod,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let mut definition = backend.registry.get(registered_node_id)?;
    let NodeProperties::Microphone {
        capture_method: configured_method,
        ..
    } = &mut definition.properties
    else {
        return Err(BackendError::InvalidOperation(
            "capture method can only be changed for microphone inputs".into(),
        ));
    };
    *configured_method = capture_method;
    let definition = backend.registry.update(definition)?;

    let instances: Vec<_> = backend
        .controller
        .snapshot()
        .nodes
        .values()
        .filter(|node| node.registered_node_id == Some(registered_node_id))
        .cloned()
        .collect();
    for mut node in instances {
        let was_active = backend.nodes.is_active(node.id);
        if was_active {
            backend.nodes.stop_node(node.id)?;
        }
        let NodeProperties::Microphone {
            capture_method: configured_method,
            ..
        } = &mut node.properties
        else {
            continue;
        };
        *configured_method = capture_method;
        let node = backend.controller.update_node(node)?;
        if was_active && node.runtime_enabled {
            backend.nodes.start_node(node.id)?;
        }
    }
    save_current_configuration(backend)?;
    Ok(definition)
}

pub fn reconnect_input_node(backend: &Backend, node_id: NodeId) -> Result<NodeStartResult> {
    let _operation = backend.operation_lock.lock();
    let node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    if !matches!(node.properties, NodeProperties::Microphone { .. }) {
        return Err(BackendError::InvalidOperation(
            "only microphone input nodes can be reconnected".into(),
        ));
    }
    if backend.nodes.is_active(node_id) {
        backend.nodes.stop_node(node_id)?;
    }
    backend.nodes.start_node(node_id)
}

pub fn set_registered_node_enabled(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    enabled: bool,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend.registry.get(registered_node_id)?;
    if node.node_type.class() == crate::audio::graph::NodeClass::Output {
        ensure_no_graph_audition(backend)?;
    }
    node.enabled = enabled;
    let node = backend.registry.update(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn update_registered_node_comment(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    comment: String,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend.registry.get(registered_node_id)?;
    node.comment = comment;
    let node = backend.registry.update(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_show_in_playback(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    show: bool,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend.registry.get(registered_node_id)?;
    node.show_in_playback = show;
    let node = backend.registry.update(node)?;
    backend
        .controller
        .set_registered_playback_display(registered_node_id, show)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_playback_order(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
    order: i32,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend.registry.get(registered_node_id)?;
    node.playback_order = order;
    let node = backend.registry.update(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn update_node_comment(
    backend: &Backend,
    node_id: NodeId,
    comment: String,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    node.comment = comment;
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_graph_playback_display(
    backend: &Backend,
    node_id: NodeId,
    show_in_playback: bool,
    playback_order: i32,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    node.show_in_playback = show_in_playback;
    node.playback_order = playback_order;
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn delete_registered_node(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    if backend
        .controller
        .snapshot()
        .nodes
        .values()
        .any(|node| node.registered_node_id == Some(registered_node_id))
    {
        return Err(BackendError::InvalidOperation(
            "remove graph instance before deleting its registered definition".into(),
        ));
    }
    backend.registry.delete(registered_node_id)?;
    save_current_configuration(backend)
}

pub fn duplicate_registered_node(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let duplicate = backend.registry.duplicate(registered_node_id)?;
    save_current_configuration(backend)?;
    Ok(duplicate)
}

pub fn add_graph_node(backend: &Backend, registered_node_id: RegisteredNodeId) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let node = backend.registry.instantiate(registered_node_id, &graph)?;
    ensure_memory_replay_capacity(backend, &node.properties)?;
    let node = backend.controller.create_node(node)?;
    // 画面からの「登録して即配置」は、既にグラフが稼働中でもこの経路を通る。
    // 以前はGraphだけが追加されprocessorがbindされず、TransformFailurePolicy::Silence
    // によりコンプレッサー／ダッキングが無音になっていた。
    if backend.runtime.is_running() {
        if let Err(error) = backend.nodes.start_node(node.id) {
            let _ = backend.controller.delete_node(node.id);
            return Err(error);
        }
    }
    save_current_configuration(backend)?;
    Ok(node)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlaybackDevicePlacementRequest {
    pub registered_node_id: RegisteredNodeId,
    pub x: f32,
    pub y: f32,
    pub operation_id: uuid::Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlaybackDevicePlacementResult {
    pub node: NodeSpec,
    pub layout: NodeLayout,
    pub graph_revision: u64,
    pub registration_enabled: bool,
    pub outcome: String,
}

/// Atomically enables a connected render registration and places its Monitor
/// node. Repeating a request never creates a second instance; this is also the
/// safe recovery path for an ambiguous frontend response.
pub fn place_playback_device(
    backend: &Backend,
    request: PlaybackDevicePlacementRequest,
) -> Result<PlaybackDevicePlacementResult> {
    let _operation = backend.operation_lock.lock();
    if !request.x.is_finite() || !request.y.is_finite() {
        return Err(BackendError::InvalidOperation(
            "初期配置座標が不正です".into(),
        ));
    }
    let definition = backend.registry.get(request.registered_node_id)?;
    if let Some(node_id) = backend
        .completed_playback_device_operations
        .lock()
        .get(&request.operation_id)
        .copied()
    {
        let graph = backend.controller.snapshot();
        if let Some(node) = graph.nodes.get(&node_id).cloned() {
            if node.registered_node_id != Some(request.registered_node_id) {
                return Err(BackendError::InvalidOperation(
                    "操作IDが別の再生デバイスに使用されています".into(),
                ));
            }
            let layout = graph
                .layouts
                .get(&node_id)
                .copied()
                .unwrap_or(NodeLayout::new(node_id, request.x, request.y));
            return Ok(PlaybackDevicePlacementResult {
                node,
                layout,
                graph_revision: graph.revision,
                registration_enabled: definition.enabled,
                outcome: "replayed".into(),
            });
        }
    }
    if backend
        .completed_playback_device_operations
        .lock()
        .contains_key(&request.operation_id)
    {
        return Err(BackendError::InvalidOperation(
            "この操作で追加したノードは削除済み、または別プロジェクトです".into(),
        ));
    }
    if definition.node_type != NodeType::Monitor {
        return Err(BackendError::InvalidOperation(
            "再生デバイス登録ではありません".into(),
        ));
    }
    let endpoint_id = match &definition.properties {
        NodeProperties::Monitor { endpoint_id, .. } => endpoint_id,
        _ => {
            return Err(BackendError::InvalidOperation(
                "再生デバイスEndpointが不正です".into(),
            ))
        }
    };
    let resolved_endpoint_id = resolve_endpoint_id(endpoint_id, DeviceDirection::Render)?;
    let device = backend
        .device_history
        .read()
        .devices
        .values()
        .find(|device| {
            device.endpoint_id == resolved_endpoint_id
                && device.direction == crate::windows::devices::DeviceDirection::Render
        })
        .cloned()
        .ok_or_else(|| BackendError::InvalidOperation("再生デバイスが見つかりません".into()))?;
    if device.connection_state != DeviceConnectionState::Connected {
        return Err(BackendError::InvalidOperation(
            "Windows再生デバイスは未接続です".into(),
        ));
    }
    let graph_before = backend.controller.snapshot();
    if let Some(node) = graph_before
        .nodes
        .values()
        .find(|node| node.registered_node_id == Some(request.registered_node_id))
        .cloned()
    {
        let layout = graph_before
            .layouts
            .get(&node.id)
            .copied()
            .unwrap_or(NodeLayout::new(node.id, request.x, request.y));
        return Ok(PlaybackDevicePlacementResult {
            node,
            layout,
            graph_revision: graph_before.revision,
            registration_enabled: definition.enabled,
            outcome: "already_placed".into(),
        });
    }
    // 変更候補だけを構築する。commit以前にRegistry/History/Runtimeへ公開しない。
    let mut registry = backend.registry.snapshot();
    let mut enabled = definition.clone();
    enabled.enabled = true;
    registry.insert(enabled.id, enabled);
    let staging = crate::registry::node_registry::NodeRegistry::from_nodes(registry.clone());
    let node = staging.instantiate(request.registered_node_id, &graph_before)?;
    let layout = NodeLayout::new(node.id, request.x, request.y);
    let mut candidate = graph_before.clone();
    candidate.nodes.insert(node.id, node.clone());
    candidate.layouts.insert(node.id, layout);
    let mut history = backend.device_history.read().clone();
    history.set_usage_enabled(device.app_device_id, DeviceUsage::Monitor, true)?;
    let mut operations = backend.completed_playback_device_operations.lock().clone();
    operations.insert(request.operation_id, node.id);
    let mut persisted = backend.config.load()?;
    persisted.registered_nodes = registry.clone();
    persisted.device_history = history.clone();
    persisted.completed_playback_device_operations = operations.clone();
    persisted.active_routing_profile = backend.active_routing_profile.lock().clone();
    persisted.obs_connections = backend.action_engine.obs_connections();
    persisted.shortcuts = backend.shortcut_engine.snapshot();
    persisted.replay_cache_metadata = backend.replay_caches.snapshot();
    backend
        .controller
        .commit_graph_candidate(graph_before.revision, candidate, |graph| {
            persisted.nodes = graph.nodes.clone();
            for spec in persisted.nodes.values_mut() {
                spec.state = RuntimeState::Stopped;
            }
            persisted.routing_edges = graph.edges.clone();
            persisted.event_edges = graph.event_edges.clone();
            persisted.node_layouts = graph.layouts.clone();
            persist_configuration_candidate(backend, persisted)?;
            backend.registry.replace_all(registry);
            *backend.device_history.write() = history;
            *backend.completed_playback_device_operations.lock() = operations;
            Ok(())
        })?;
    Ok(PlaybackDevicePlacementResult {
        node,
        layout,
        graph_revision: backend.controller.snapshot().revision,
        registration_enabled: true,
        outcome: "created".into(),
    })
}

pub fn create_memory_replay_instance(
    backend: &Backend,
    cache_id: CacheId,
    mode: MemoryReplayMode,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    backend.replay_caches.get_cache(cache_id)?;
    let properties = NodeProperties::MemoryReplay {
        cache_id,
        mode,
        selected_item_id: None,
        gain_db: 0.0,
        mute: false,
    };
    ensure_memory_replay_capacity(backend, &properties)?;
    let display_name = match mode {
        MemoryReplayMode::SelectFromCache => "メモリー一覧",
        MemoryReplayMode::LatestFromCache => "最新メモリー",
    };
    let node = backend.controller.create_node(NodeSpec::new(
        NodeType::MemoryReplay,
        display_name,
        properties,
    ))?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn remove_graph_node(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    if backend.nodes.is_active(node_id) {
        backend.nodes.stop_node(node_id)?;
    }
    backend.controller.delete_node(node_id)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)
}

pub fn duplicate_graph_node(backend: &Backend, node_id: NodeId) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let layout = graph
        .layouts
        .get(&node_id)
        .copied()
        .unwrap_or(NodeLayout::new(node_id, 0.0, 0.0));
    let payload = export_graph_nodes(backend, vec![node_id])?;
    let mut pasted = paste_graph_nodes(
        backend,
        payload,
        GraphPosition {
            x: layout.x + 16.0,
            y: layout.y + 16.0,
        },
    )?;
    pasted
        .pop()
        .ok_or_else(|| BackendError::InvalidOperation("複製対象がありません".into()))
}

pub fn create_multi_input_instance(backend: &Backend, display_name: String) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut definition = RegisteredNode::user(
        display_name,
        NodeProperties::MultiInput {
            slots: Vec::new(),
            legacy_ducking: None,
            gain_db: 0.0,
            mute: false,
        },
    );
    definition.instance_policy = InstancePolicy::UnlimitedInstances;
    let definition = backend.registry.register(definition)?;
    add_graph_node(backend, definition.id)
}

pub fn get_instance_policy(
    backend: &Backend,
    registered_node_id: RegisteredNodeId,
) -> Result<InstancePolicy> {
    let _operation = backend.operation_lock.lock();
    Ok(backend.registry.get(registered_node_id)?.instance_policy)
}

pub fn set_graph_runtime_enabled(
    backend: &Backend,
    node_id: NodeId,
    enabled: bool,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node_id)?;
    let node = backend.controller.set_runtime_enabled(node_id, enabled)?;
    if !enabled && backend.nodes.is_active(node_id) {
        backend.nodes.stop_node(node_id)?;
    }
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn create_node(backend: &Backend, mut node: NodeSpec) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let migrated_virtual_device = normalize_legacy_virtual_device_node(&mut node);
    validate_audio_processing_group(backend, &node)?;
    ensure_memory_replay_capacity(backend, &node.properties)?;
    let out = backend.controller.create_node(node)?;
    if migrated_virtual_device {
        synchronize_legacy_virtual_device_registration(backend, &out)?;
    }
    save_current_configuration(backend)?;
    Ok(out)
}
pub fn update_node(backend: &Backend, mut node: NodeSpec) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node.id)?;
    let migrated_virtual_device = normalize_legacy_virtual_device_node(&mut node);
    validate_audio_processing_group(backend, &node)?;
    let out = backend.controller.update_node(node)?;
    if migrated_virtual_device {
        synchronize_legacy_virtual_device_registration(backend, &out)?;
    }
    save_current_configuration(backend)?;
    Ok(out)
}

fn validate_audio_processing_group(backend: &Backend, node: &NodeSpec) -> Result<()> {
    let NodeProperties::AudioProcessingGroup { flow, .. } = &node.properties else {
        return Ok(());
    };
    if node.node_type != NodeType::AudioProcessingGroup
        || node.input_ports.len() != 1
        || node.output_ports.len() != 1
    {
        return Err(BackendError::InvalidOperation(
            "音声処理グループは1入力1出力でなければなりません".into(),
        ));
    }
    let mut ids = std::collections::HashSet::new();
    let mut instances = std::collections::HashSet::new();
    for member in flow {
        if !ids.insert(member.id) || !instances.insert(member.instance_id) {
            return Err(BackendError::InvalidOperation(
                "グループ内の処理IDが重複しています".into(),
            ));
        }
        let descriptor = backend.plugins.descriptor(&member.plugin_id)?;
        if descriptor.execution_mode != PluginExecutionMode::BuiltIn
            || descriptor.api_version != crate::plugins::manifest::TRANSFORM_PLUGIN_API_VERSION
            || descriptor.input_ports.len() != 1
            || descriptor.output_ports.len() != 1
        {
            return Err(BackendError::InvalidOperation(
                "グループに対応しない音声処理が含まれています".into(),
            ));
        }
        backend
            .plugins
            .validate_parameters(&member.plugin_id, &member.parameters)?;
    }
    Ok(())
}

fn normalize_legacy_virtual_device_node(node: &mut NodeSpec) -> bool {
    let NodeProperties::VirtualDevice {
        endpoint_id,
        gain_db,
        mute,
    } = node.properties.clone()
    else {
        return false;
    };
    node.node_type = NodeType::Monitor;
    node.properties = NodeProperties::Monitor {
        endpoint_id,
        gain_db,
        mute,
    };
    true
}

fn synchronize_legacy_virtual_device_registration(
    backend: &Backend,
    node: &NodeSpec,
) -> Result<()> {
    let Some(registered_node_id) = node.registered_node_id else {
        return Ok(());
    };
    let Ok(mut registered) = backend.registry.get(registered_node_id) else {
        return Ok(());
    };
    registered.node_type = NodeType::Monitor;
    registered.is_virtual = true;
    if let NodeProperties::VirtualDevice {
        endpoint_id,
        gain_db,
        mute,
    } = registered.properties.clone()
    {
        registered.properties = NodeProperties::Monitor {
            endpoint_id,
            gain_db,
            mute,
        };
    }
    backend.registry.update(registered)?;
    Ok(())
}
pub fn delete_node(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    if backend.nodes.is_active(node_id) {
        backend.nodes.stop_node(node_id)?;
    }
    backend.controller.delete_node(node_id)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)
}
pub fn list_nodes(backend: &Backend) -> Vec<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    backend.controller.list_nodes()
}

/// Canvas coordinate supplied by the routing editor. Unlike NodeLayout it has
/// no caller-controlled node identity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GraphPosition {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GraphClipboard {
    pub format: String,
    pub version: u32,
    pub nodes: Vec<NodeSpec>,
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub event_edges: Vec<EventEdge>,
    #[serde(default)]
    pub layouts: Vec<NodeLayout>,
}

fn group_member_from_node(node: &NodeSpec) -> Result<GroupProcessorSpec> {
    let NodeProperties::TransformPlugin {
        plugin_id,
        instance_id,
        parameters,
        bypass,
        failure_policy,
        switch_policy,
        gain_db,
        mute,
    } = &node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "音声処理プラグインではありません".into(),
        ));
    };
    Ok(GroupProcessorSpec {
        id: GroupProcessorId::new(),
        display_name: node.display_name.clone(),
        plugin_id: plugin_id.clone(),
        instance_id: *instance_id,
        parameters: parameters.clone(),
        bypass: *bypass,
        failure_policy: *failure_policy,
        switch_policy: *switch_policy,
        gain_db: *gain_db,
        mute: *mute,
    })
}

fn ensure_groupable(backend: &Backend, node: &NodeSpec) -> Result<()> {
    if node.node_type != NodeType::TransformPlugin
        || node.input_ports.len() != 1
        || node.output_ports.len() != 1
    {
        return Err(BackendError::InvalidOperation(
            "グループ化できるのは1入力1出力の音声処理だけです".into(),
        ));
    }
    let NodeProperties::TransformPlugin { plugin_id, .. } = &node.properties else {
        unreachable!()
    };
    let descriptor = backend.plugins.descriptor(plugin_id)?;
    if descriptor.execution_mode != PluginExecutionMode::BuiltIn
        || descriptor.api_version != crate::plugins::manifest::TRANSFORM_PLUGIN_API_VERSION
        || descriptor.input_ports.len() != 1
        || descriptor.output_ports.len() != 1
    {
        return Err(BackendError::InvalidOperation(
            "この音声処理はグループに対応していません".into(),
        ));
    }
    Ok(())
}

pub fn create_audio_processing_group(
    backend: &Backend,
    node_ids: Vec<NodeId>,
    position: GraphPosition,
    ordered_node_ids: Option<Vec<NodeId>>,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    if node_ids.is_empty() {
        return Err(BackendError::InvalidOperation(
            "グループ化するノードを選択してください".into(),
        ));
    }
    let graph = backend.controller.snapshot();
    let selected: std::collections::HashSet<_> = node_ids.iter().copied().collect();
    if selected.len() != node_ids.len() {
        return Err(BackendError::InvalidOperation(
            "グループ化対象が重複しています".into(),
        ));
    }
    let mut order = ordered_node_ids.unwrap_or(node_ids.clone());
    if order.len() != selected.len()
        || order.iter().any(|id| !selected.contains(id))
        || order
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != selected.len()
    {
        return Err(BackendError::InvalidOperation(
            "グループ内の処理順が不正です".into(),
        ));
    }
    let internal: Vec<_> = graph
        .edges
        .iter()
        .filter(|edge| selected.contains(&edge.from_node) && selected.contains(&edge.to_node))
        .collect();
    for edge in &internal {
        if edge.gain_db != 0.0 || edge.mute {
            return Err(BackendError::InvalidOperation(
                "音量またはミュート設定を持つ接続はグループ化できません".into(),
            ));
        }
    }
    // Each connected component must be a simple directed chain. Components
    // are ordered by their first appearance in the UI order; a component's
    // own order always follows its existing audio connection.
    let mut next = HashMap::new();
    let mut previous = HashMap::new();
    for edge in &internal {
        if next.insert(edge.from_node, edge.to_node).is_some()
            || previous.insert(edge.to_node, edge.from_node).is_some()
        {
            return Err(BackendError::InvalidOperation(
                "分岐を含む接続はグループ化できません".into(),
            ));
        }
    }
    let requested = order.clone();
    let mut rebuilt = Vec::with_capacity(requested.len());
    let mut visited = std::collections::HashSet::new();
    for requested_id in requested {
        if visited.contains(&requested_id) {
            continue;
        }
        let mut start = requested_id;
        let mut backwards = std::collections::HashSet::new();
        while let Some(before) = previous.get(&start) {
            if !backwards.insert(start) {
                return Err(BackendError::InvalidOperation(
                    "循環接続はグループ化できません".into(),
                ));
            }
            start = *before;
        }
        let mut current = start;
        loop {
            if !visited.insert(current) {
                return Err(BackendError::InvalidOperation(
                    "循環接続はグループ化できません".into(),
                ));
            }
            rebuilt.push(current);
            let Some(after) = next.get(&current) else {
                break;
            };
            current = *after;
        }
    }
    if rebuilt.len() != selected.len() {
        return Err(BackendError::InvalidOperation(
            "接続済みの直列処理順を特定できません".into(),
        ));
    }
    order = rebuilt;
    let nodes: Vec<_> = order
        .iter()
        .map(|id| {
            graph
                .nodes
                .get(id)
                .cloned()
                .ok_or_else(|| BackendError::NodeNotFound(id.to_string()))
        })
        .collect::<Result<_>>()?;
    for node in &nodes {
        ensure_groupable(backend, node)?;
    }
    let incoming: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| selected.contains(&e.to_node) && !selected.contains(&e.from_node))
        .cloned()
        .collect();
    let outgoing: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| selected.contains(&e.from_node) && !selected.contains(&e.to_node))
        .cloned()
        .collect();
    if incoming.len() > 1 || outgoing.len() > 1 {
        return Err(BackendError::InvalidOperation(
            "外部接続が1入力1出力にならないためグループ化できません".into(),
        ));
    }
    if incoming
        .first()
        .is_some_and(|edge| edge.to_node != order[0])
        || outgoing
            .last()
            .is_some_and(|edge| edge.from_node != *order.last().unwrap())
    {
        return Err(BackendError::InvalidOperation(
            "外部接続の始点または終点を保持できないためグループ化できません".into(),
        ));
    }
    let group = NodeSpec::new(
        NodeType::AudioProcessingGroup,
        "音声処理グループ",
        NodeProperties::AudioProcessingGroup {
            flow: nodes
                .iter()
                .map(group_member_from_node)
                .collect::<Result<_>>()?,
            gain_db: 0.0,
            mute: false,
        },
    );
    let group_id = group.id;
    let mut candidate = graph.clone();
    for id in &selected {
        candidate.nodes.remove(id);
        candidate.layouts.remove(id);
    }
    candidate
        .edges
        .retain(|e| !selected.contains(&e.from_node) && !selected.contains(&e.to_node));
    if let Some(mut edge) = incoming.into_iter().next() {
        edge.to_node = group_id;
        edge.to_port = group.input_ports[0].id;
        candidate.edges.push(edge);
    }
    if let Some(mut edge) = outgoing.into_iter().next() {
        edge.from_node = group_id;
        edge.from_port = group.output_ports[0].id;
        candidate.edges.push(edge);
    }
    candidate
        .layouts
        .insert(group_id, NodeLayout::new(group_id, position.x, position.y));
    candidate.nodes.insert(group_id, group.clone());
    let active_before: Vec<_> = selected
        .iter()
        .copied()
        .filter(|id| backend.nodes.is_active(*id))
        .collect();
    for id in &active_before {
        if backend.nodes.is_active(*id) {
            backend.nodes.stop_node(*id)?;
        }
    }
    let committed = backend.controller.commit_graph_candidate(
        backend.controller.snapshot().revision,
        candidate,
        |next| save_graph_configuration(backend, next.clone()),
    );
    if let Err(error) = committed {
        for id in active_before {
            let _ = backend.nodes.start_node(id);
        }
        return Err(error);
    }
    if !active_before.is_empty() {
        backend.nodes.start_node(group_id)?;
    }
    Ok(group)
}

pub fn set_audio_processing_group(
    backend: &Backend,
    node_id: NodeId,
    flow: Vec<GroupProcessorSpec>,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let mut group = graph
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::AudioProcessingGroup { flow: stored, .. } = &mut group.properties else {
        return Err(BackendError::InvalidOperation(
            "音声処理グループではありません".into(),
        ));
    };
    let mut ids = std::collections::HashSet::new();
    let mut instances = std::collections::HashSet::new();
    for member in &flow {
        if !ids.insert(member.id) || !instances.insert(member.instance_id) {
            return Err(BackendError::InvalidOperation(
                "グループ内の処理IDが重複しています".into(),
            ));
        }
        let descriptor = backend.plugins.descriptor(&member.plugin_id)?;
        if descriptor.execution_mode != PluginExecutionMode::BuiltIn
            || descriptor.api_version != crate::plugins::manifest::TRANSFORM_PLUGIN_API_VERSION
            || descriptor.input_ports.len() != 1
            || descriptor.output_ports.len() != 1
        {
            return Err(BackendError::InvalidOperation(
                "グループに対応しない音声処理が含まれています".into(),
            ));
        }
        backend
            .plugins
            .validate_parameters(&member.plugin_id, &member.parameters)?;
    }
    *stored = flow;
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    let mut candidate = graph.clone();
    candidate.nodes.insert(node_id, group.clone());
    let result = backend.controller.commit_graph_candidate(
        backend.controller.snapshot().revision,
        candidate,
        |next| save_graph_configuration(backend, next.clone()),
    );
    if let Err(error) = result {
        if was_active {
            let _ = backend.nodes.start_node(node_id);
        }
        return Err(error);
    }
    if was_active {
        backend.nodes.start_node(node_id)?;
    }
    Ok(group)
}

pub fn ungroup_audio_processing(
    backend: &Backend,
    node_id: NodeId,
    grid_spacing: f32,
) -> Result<Vec<NodeSpec>> {
    let _operation = backend.operation_lock.lock();
    if !grid_spacing.is_finite() || grid_spacing < 0.0 {
        return Err(BackendError::InvalidOperation(
            "グリッド間隔が不正です".into(),
        ));
    }
    let graph = backend.controller.snapshot();
    let group = graph
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::AudioProcessingGroup { flow, .. } = &group.properties else {
        return Err(BackendError::InvalidOperation(
            "音声処理グループではありません".into(),
        ));
    };
    if flow.is_empty() {
        return Err(BackendError::InvalidOperation(
            "空グループには分解する処理がありません。素通りノードとして保持します".into(),
        ));
    }
    let mut nodes = Vec::with_capacity(flow.len());
    for member in flow {
        let node = NodeSpec::new(
            NodeType::TransformPlugin,
            member.display_name.clone(),
            NodeProperties::TransformPlugin {
                plugin_id: member.plugin_id.clone(),
                instance_id: member.instance_id,
                parameters: member.parameters.clone(),
                bypass: member.bypass,
                failure_policy: member.failure_policy,
                switch_policy: member.switch_policy,
                gain_db: member.gain_db,
                mute: member.mute,
            },
        );
        nodes.push(node);
    }
    let mut candidate = graph.clone();
    candidate.nodes.remove(&node_id);
    candidate.layouts.remove(&node_id);
    let incoming: Vec<_> = candidate
        .edges
        .iter()
        .filter(|edge| edge.to_node == node_id)
        .cloned()
        .collect();
    let outgoing: Vec<_> = candidate
        .edges
        .iter()
        .filter(|edge| edge.from_node == node_id)
        .cloned()
        .collect();
    candidate
        .edges
        .retain(|edge| edge.to_node != node_id && edge.from_node != node_id);
    if !nodes.is_empty() {
        let origin = graph
            .layouts
            .get(&node_id)
            .copied()
            .unwrap_or(NodeLayout::new(node_id, 0.0, 0.0));
        for (i, node) in nodes.iter().enumerate() {
            candidate.layouts.insert(
                node.id,
                NodeLayout::new(
                    node.id,
                    origin.x + grid_spacing * i as f32,
                    origin.y + grid_spacing * i as f32,
                ),
            );
            candidate.nodes.insert(node.id, node.clone());
        }
        for mut edge in incoming {
            edge.to_node = nodes[0].id;
            edge.to_port = nodes[0].input_ports[0].id;
            candidate.edges.push(edge);
        }
        for pair in nodes.windows(2) {
            candidate.edges.push(Edge::new(
                pair[0].id,
                pair[0].output_ports[0].id,
                pair[1].id,
                pair[1].input_ports[0].id,
            ));
        }
        for mut edge in outgoing {
            let last = nodes.last().unwrap();
            edge.from_node = last.id;
            edge.from_port = last.output_ports[0].id;
            candidate.edges.push(edge);
        }
    } else {
        // Empty groups are valid pass-through nodes; ungrouping one simply
        // removes it and reconnects its sole external route when possible.
        if incoming.len() == 1 && outgoing.len() == 1 {
            let mut edge = incoming[0].clone();
            edge.to_node = outgoing[0].to_node;
            edge.to_port = outgoing[0].to_port;
            candidate.edges.push(edge);
        }
    }
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    let committed = backend.controller.commit_graph_candidate(
        backend.controller.snapshot().revision,
        candidate,
        |next| save_graph_configuration(backend, next.clone()),
    );
    if let Err(error) = committed {
        if was_active {
            let _ = backend.nodes.start_node(node_id);
        }
        return Err(error);
    }
    if was_active {
        for node in &nodes {
            backend.nodes.start_node(node.id)?;
        }
    }
    Ok(nodes)
}

pub fn set_audio_processing_group_processor_parameter(
    backend: &Backend,
    node_id: NodeId,
    processor_id: GroupProcessorId,
    parameter_id: String,
    value: serde_json::Value,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut group = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::AudioProcessingGroup { flow, .. } = &mut group.properties else {
        return Err(BackendError::InvalidOperation(
            "音声処理グループではありません".into(),
        ));
    };
    let member = flow
        .iter_mut()
        .find(|member| member.id == processor_id)
        .ok_or_else(|| {
            BackendError::InvalidOperation("グループ内の音声処理が見つかりません".into())
        })?;
    backend
        .plugins
        .validate_parameter(&member.plugin_id, &parameter_id, &value)?;
    member.parameters.insert(parameter_id, value);
    let NodeProperties::AudioProcessingGroup { flow, .. } = group.properties else {
        unreachable!()
    };
    set_audio_processing_group(backend, node_id, flow)
}

pub fn export_graph_nodes(backend: &Backend, node_ids: Vec<NodeId>) -> Result<serde_json::Value> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let selected: std::collections::HashSet<_> = node_ids.into_iter().collect();
    let nodes = selected
        .iter()
        .map(|id| {
            graph
                .nodes
                .get(id)
                .cloned()
                .ok_or_else(|| BackendError::NodeNotFound(id.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let edges = graph
        .edges
        .iter()
        .filter(|edge| selected.contains(&edge.from_node) && selected.contains(&edge.to_node))
        .cloned()
        .collect();
    let layouts = selected
        .iter()
        .filter_map(|id| graph.layouts.get(id).copied())
        .collect();
    let event_edges = graph
        .event_edges
        .iter()
        .filter(|edge| selected.contains(&edge.from_node) && selected.contains(&edge.to_node))
        .cloned()
        .collect();
    Ok(serde_json::to_value(GraphClipboard {
        format: "voicemixer.nodes".into(),
        version: 1,
        nodes,
        edges,
        event_edges,
        layouts,
    })?)
}

pub fn paste_graph_nodes(
    backend: &Backend,
    payload: serde_json::Value,
    position: GraphPosition,
) -> Result<Vec<NodeSpec>> {
    let _operation = backend.operation_lock.lock();
    if !position.x.is_finite() || !position.y.is_finite() {
        return Err(BackendError::InvalidOperation(
            "貼り付け位置が不正です".into(),
        ));
    }
    let clipboard: GraphClipboard = serde_json::from_value(payload).map_err(|error| {
        BackendError::InvalidOperation(format!("貼り付けデータの形式が不正です: {error}"))
    })?;
    if clipboard.format != "voicemixer.nodes"
        || clipboard.version != 1
        || clipboard.nodes.is_empty()
    {
        return Err(BackendError::InvalidOperation(
            "対応していない貼り付けデータです".into(),
        ));
    }
    let graph = backend.controller.snapshot();
    let mut old_node_ids = std::collections::HashSet::new();
    let mut old_port_ids = std::collections::HashSet::new();
    let mut node_map = HashMap::new();
    let mut port_map = HashMap::new();
    let mut copied = clipboard.nodes.clone();
    let mut pasted_registered = std::collections::HashSet::new();
    let is_latest_memory = |node: &NodeSpec| {
        matches!(
            &node.properties,
            NodeProperties::MemoryReplay {
                mode: MemoryReplayMode::LatestFromCache,
                ..
            }
        )
    };
    if copied.iter().any(is_latest_memory)
        && graph
            .nodes
            .values()
            .chain(copied.iter())
            .filter(|node| is_latest_memory(node))
            .count()
            > 1
    {
        return Err(BackendError::InvalidOperation(
            "貼り付けに失敗しました。最新メモリーは1つだけ配置できます".into(),
        ));
    }
    for node in &mut copied {
        if node.node_type != node.properties.node_type() {
            return Err(BackendError::InvalidOperation(
                "貼り付けノードの種類と設定が一致しません".into(),
            ));
        }
        // 再採番で不正な内部ID重複が隠れないよう、受信時のフローも検査する。
        validate_audio_processing_group(backend, node)?;
        if let NodeProperties::TransformPlugin {
            plugin_id,
            parameters,
            ..
        } = &node.properties
        {
            backend.plugins.validate_parameters(plugin_id, parameters)?;
        }
        let registered_identity = backend
            .registry
            .registered_identity_for_import(node, &graph);
        if registered_identity.is_none()
            && (node.registered_node_id.is_some()
                || !crate::registry::node_registry::NodeRegistry::allows_unregistered_import(node))
        {
            return Err(BackendError::InvalidOperation(format!(
                "貼り付けに失敗しました。ノード『{}』の登録元または設置数制限を確認できません。ノード管理の定義と一致するJSONを使用してください",
                node.display_name
            )));
        }
        if !old_node_ids.insert(node.id) {
            return Err(BackendError::InvalidOperation(
                "貼り付けデータに重複ノードIDがあります".into(),
            ));
        }
        let old = node.id;
        node.id = NodeId::new();
        node.state = RuntimeState::Stopped;
        node_map.insert(old, node.id);
        for port in node
            .input_ports
            .iter_mut()
            .chain(node.output_ports.iter_mut())
        {
            if !old_port_ids.insert(port.id) {
                return Err(BackendError::InvalidOperation(
                    "貼り付けデータに重複ポートIDがあります".into(),
                ));
            }
            let old = port.id;
            port.id = PortId::new();
            port_map.insert(old, port.id);
        }
        match &mut node.properties {
            NodeProperties::TransformPlugin { instance_id, .. } => {
                *instance_id = PluginInstanceId::new()
            }
            NodeProperties::Rvc { instance_id, .. } => *instance_id = RvcInstanceId::new(),
            NodeProperties::AudioProcessingGroup { flow, .. } => {
                for member in flow {
                    member.id = GroupProcessorId::new();
                    member.instance_id = PluginInstanceId::new();
                }
            }
            _ => {}
        }
        if let Some(registered_id) = registered_identity {
            let registered = backend.registry.get(registered_id)?;
            if !registered.enabled {
                return Err(BackendError::InvalidOperation(format!(
                    "貼り付けに失敗しました。ノード『{}』の登録が無効です",
                    registered.display_name
                )));
            }
            if registered.instance_policy == InstancePolicy::RegisteredInstanceOnly
                && (!pasted_registered.insert(registered_id)
                    || graph
                        .nodes
                        .values()
                        .any(|existing| existing.registered_node_id == Some(registered_id)))
            {
                return Err(BackendError::InvalidOperation(
                    "重複設置できないため貼り付けに失敗しました".into(),
                ));
            }
            node.registered_node_id = Some(registered_id);
        }
    }
    let mut candidate = graph.clone();
    let edge_id_map: HashMap<_, _> = clipboard
        .edges
        .iter()
        .map(|edge| (edge.id, EdgeId::new()))
        .collect();
    for node in &mut copied {
        remap_pasted_node_references(node, &node_map, &port_map, &edge_id_map);
        validate_audio_processing_group(backend, node)?;
    }
    let mut edges = Vec::with_capacity(clipboard.edges.len());
    for mut edge in clipboard.edges {
        edge.from_node = *node_map.get(&edge.from_node).ok_or_else(|| {
            BackendError::InvalidOperation("貼り付け接続の始点が含まれていません".into())
        })?;
        edge.to_node = *node_map.get(&edge.to_node).ok_or_else(|| {
            BackendError::InvalidOperation("貼り付け接続の終点が含まれていません".into())
        })?;
        edge.from_port = *port_map
            .get(&edge.from_port)
            .ok_or_else(|| BackendError::InvalidOperation("貼り付け接続のPortが不正です".into()))?;
        edge.to_port = *port_map
            .get(&edge.to_port)
            .ok_or_else(|| BackendError::InvalidOperation("貼り付け接続のPortが不正です".into()))?;
        edge.id = *edge_id_map.get(&edge.id).unwrap();
        edges.push(edge);
    }
    let mut event_edges = Vec::with_capacity(clipboard.event_edges.len());
    for mut edge in clipboard.event_edges {
        edge.from_node = *node_map.get(&edge.from_node).ok_or_else(|| {
            BackendError::InvalidOperation("貼り付けイベント接続の始点が含まれていません".into())
        })?;
        edge.to_node = *node_map.get(&edge.to_node).ok_or_else(|| {
            BackendError::InvalidOperation("貼り付けイベント接続の終点が含まれていません".into())
        })?;
        edge.from_port = *port_map.get(&edge.from_port).ok_or_else(|| {
            BackendError::InvalidOperation("貼り付けイベント接続のPortが不正です".into())
        })?;
        edge.to_port = *port_map.get(&edge.to_port).ok_or_else(|| {
            BackendError::InvalidOperation("貼り付けイベント接続のPortが不正です".into())
        })?;
        edge.id = EdgeId::new();
        event_edges.push(edge);
    }
    let old_layouts: HashMap<_, _> = clipboard
        .layouts
        .into_iter()
        .map(|layout| (layout.node_id, layout))
        .collect();
    let min_x = old_layouts
        .values()
        .map(|layout| layout.x)
        .reduce(f32::min)
        .unwrap_or(0.0);
    let min_y = old_layouts
        .values()
        .map(|layout| layout.y)
        .reduce(f32::min)
        .unwrap_or(0.0);
    for (index, node) in copied.iter().enumerate() {
        let old_id = node_map
            .iter()
            .find_map(|(old, new)| (*new == node.id).then_some(*old))
            .unwrap();
        let old_layout = old_layouts.get(&old_id).copied().unwrap_or(NodeLayout::new(
            old_id,
            index as f32 * 16.0,
            index as f32 * 16.0,
        ));
        candidate.layouts.insert(
            node.id,
            NodeLayout::new(
                node.id,
                position.x + old_layout.x - min_x,
                position.y + old_layout.y - min_y,
            ),
        );
        candidate.nodes.insert(node.id, node.clone());
    }
    candidate.edges.extend(edges);
    candidate.event_edges.extend(event_edges);
    backend
        .controller
        .commit_graph_candidate(graph.revision, candidate, |next| {
            save_graph_configuration(backend, next.clone())
        })?;
    Ok(copied)
}

fn remap_pasted_node_references(
    node: &mut NodeSpec,
    node_map: &HashMap<NodeId, NodeId>,
    port_map: &HashMap<PortId, PortId>,
    edge_map: &HashMap<EdgeId, EdgeId>,
) {
    match &mut node.properties {
        NodeProperties::ReplayInput { source_node_id, .. } => {
            *source_node_id = source_node_id.and_then(|id| node_map.get(&id).copied())
        }
        NodeProperties::MultiInput { slots, .. } => {
            for slot in slots {
                slot.port_id = port_map.get(&slot.port_id).copied().unwrap_or(slot.port_id);
                slot.edge_id = slot.edge_id.and_then(|id| edge_map.get(&id).copied());
            }
        }
        NodeProperties::InputSelector { config, .. } => {
            config.active_port_id = config
                .active_port_id
                .and_then(|id| port_map.get(&id).copied());
            config.default_port_id = config
                .default_port_id
                .and_then(|id| port_map.get(&id).copied());
            for input in &mut config.inputs {
                input.port_id = port_map
                    .get(&input.port_id)
                    .copied()
                    .unwrap_or(input.port_id);
            }
        }
        NodeProperties::ChannelMapper { config, .. } => {
            for entry in &mut config.port_map {
                entry.source_port_id = port_map
                    .get(&entry.source_port_id)
                    .copied()
                    .unwrap_or(entry.source_port_id);
            }
        }
        _ => {}
    }
}

pub fn delete_graph_nodes(backend: &Backend, node_ids: Vec<NodeId>) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let selected: std::collections::HashSet<_> = node_ids.into_iter().collect();
    if selected.iter().any(|id| !graph.nodes.contains_key(id)) {
        return Err(BackendError::InvalidOperation(
            "削除対象のノードが見つかりません".into(),
        ));
    }
    let mut candidate = graph.clone();
    candidate.nodes.retain(|id, _| !selected.contains(id));
    candidate.layouts.retain(|id, _| !selected.contains(id));
    candidate
        .edges
        .retain(|edge| !selected.contains(&edge.from_node) && !selected.contains(&edge.to_node));
    candidate
        .event_edges
        .retain(|edge| !selected.contains(&edge.from_node) && !selected.contains(&edge.to_node));
    // 元のNodeSpecが存在する間に内部プラグインも解放し、保存失敗時は復元する。
    let active_before: Vec<_> = selected
        .iter()
        .copied()
        .filter(|id| backend.nodes.is_active(*id))
        .collect();
    for id in &active_before {
        backend.nodes.stop_node(*id)?;
    }
    let result = backend.controller.commit_graph_candidate(
        backend.controller.snapshot().revision,
        candidate,
        |next| save_graph_configuration(backend, next.clone()),
    );
    if let Err(error) = result {
        for id in active_before {
            let _ = backend.nodes.start_node(id);
        }
        return Err(error);
    }
    backend.nodes.refresh_recorder_inputs();
    Ok(())
}

pub fn add_multi_input_port(
    backend: &Backend,
    node_id: NodeId,
    name: String,
) -> Result<crate::audio::graph::PortId> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let port_id = node
        .add_multi_input_port(name)
        .ok_or_else(|| BackendError::InvalidOperation("node is not Multi Input".into()))?;
    backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(port_id)
}

pub fn remove_port(
    backend: &Backend,
    node_id: NodeId,
    port_id: crate::audio::graph::PortId,
) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.controller.remove_port(node_id, port_id)?;
    save_current_configuration(backend)
}

pub fn connect_nodes(
    backend: &Backend,
    from_node: NodeId,
    from_port: crate::audio::graph::PortId,
    to_node: NodeId,
    to_port: crate::audio::graph::PortId,
) -> Result<Edge> {
    let _operation = backend.operation_lock.lock();
    let edge = backend
        .controller
        .connect_nodes(from_node, from_port, to_node, to_port)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)?;
    Ok(edge)
}
pub fn connect_nodes_auto(backend: &Backend, from_node: NodeId, to_node: NodeId) -> Result<Edge> {
    let _operation = backend.operation_lock.lock();
    let edge = backend.controller.connect_nodes_auto(from_node, to_node)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)?;
    Ok(edge)
}
pub fn connect_endpoints(
    backend: &Backend,
    first: ConnectionEndpoint,
    second: ConnectionEndpoint,
) -> Result<Edge> {
    let _operation = backend.operation_lock.lock();
    let edge = backend.controller.connect_endpoints(first, second)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)?;
    Ok(edge)
}

pub fn list_event_edges(backend: &Backend) -> Vec<EventEdge> {
    let _operation = backend.operation_lock.lock();
    backend.controller.list_event_edges()
}

pub fn connect_event_nodes(
    backend: &Backend,
    from_node: NodeId,
    from_port: PortId,
    to_node: NodeId,
    to_port: PortId,
) -> Result<EventEdge> {
    let _operation = backend.operation_lock.lock();
    let edge = backend
        .controller
        .connect_event_nodes(from_node, from_port, to_node, to_port)?;
    save_current_configuration(backend)?;
    Ok(edge)
}

pub fn connect_event_endpoints(
    backend: &Backend,
    first: ConnectionEndpoint,
    second: ConnectionEndpoint,
) -> Result<EventEdge> {
    let _operation = backend.operation_lock.lock();
    let edge = backend.controller.connect_event_endpoints(first, second)?;
    save_current_configuration(backend)?;
    Ok(edge)
}
pub fn validate_connection_candidate_command(
    backend: &Backend,
    source: NodeId,
    destination: NodeId,
) -> ConnectionCandidateResult {
    let _operation = backend.operation_lock.lock();
    backend
        .controller
        .validate_connection_candidate(source, destination, CyclePolicy::WarningOnly)
}
pub fn disconnect_nodes(backend: &Backend, edge_id: EdgeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.controller.disconnect_nodes(edge_id)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)
}
pub fn disconnect_edge(backend: &Backend, edge_id: EdgeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    disconnect_nodes(backend, edge_id)
}
pub fn set_edge_controls(
    backend: &Backend,
    edge_id: EdgeId,
    gain_db: f32,
    mute: bool,
) -> Result<Edge> {
    let _operation = backend.operation_lock.lock();
    let edge = backend
        .controller
        .set_edge_controls(edge_id, gain_db, mute)?;
    save_current_configuration(backend)?;
    Ok(edge)
}
pub fn list_edges(backend: &Backend) -> Vec<Edge> {
    let _operation = backend.operation_lock.lock();
    backend.controller.list_edges()
}
pub fn set_edge_color(backend: &Backend, edge_id: EdgeId, color: Option<String>) -> Result<Edge> {
    let _operation = backend.operation_lock.lock();
    if color.as_ref().is_some_and(|value| {
        value.len() != 7
            || !value.starts_with('#')
            || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    }) {
        return Err(BackendError::InvalidOperation(
            "線の色は#RRGGBBで指定してください".into(),
        ));
    }
    let graph = backend.controller.snapshot();
    let mut candidate = graph.clone();
    let edge = candidate
        .edges
        .iter_mut()
        .find(|edge| edge.id == edge_id)
        .ok_or_else(|| BackendError::EdgeNotFound(edge_id.to_string()))?;
    edge.color = color;
    let updated = edge.clone();
    backend
        .controller
        .commit_graph_candidate(graph.revision, candidate, |candidate| {
            save_graph_configuration(backend, candidate.clone())
        })?;
    Ok(updated)
}
pub fn validate_graph(backend: &Backend) -> ValidationReport {
    let _operation = backend.operation_lock.lock();
    backend.controller.validate_graph()
}
pub fn get_upstream_nodes(backend: &Backend, node_id: NodeId) -> Result<Vec<NodeId>> {
    let _operation = backend.operation_lock.lock();
    upstream_nodes(&backend.controller.snapshot(), node_id)
}
pub fn get_downstream_nodes(backend: &Backend, node_id: NodeId) -> Result<Vec<NodeId>> {
    let _operation = backend.operation_lock.lock();
    downstream_nodes(&backend.controller.snapshot(), node_id)
}
pub fn get_related_route(backend: &Backend, node_id: NodeId) -> Result<Vec<NodeId>> {
    let _operation = backend.operation_lock.lock();
    related_route_nodes(&backend.controller.snapshot(), node_id)
}
pub fn get_route_to_output(backend: &Backend, output_node: NodeId) -> Result<RouteSubgraph> {
    let _operation = backend.operation_lock.lock();
    route_to_output(&backend.controller.snapshot(), output_node)
}
pub fn update_node_layout(backend: &Backend, layout: NodeLayout) -> Result<NodeLayout> {
    let _operation = backend.operation_lock.lock();
    let layout = backend.controller.update_node_layout(layout)?;
    save_current_configuration(backend)?;
    Ok(layout)
}

pub fn start_node(backend: &Backend, node_id: NodeId) -> Result<NodeStartResult> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node_id)?;
    backend.start_runtime()?;
    let result = backend.nodes.start_node(node_id)?;
    backend.nodes.refresh_recorder_inputs();
    Ok(result)
}
pub fn stop_node(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node_id)?;
    backend.nodes.stop_node(node_id)?;
    backend.nodes.refresh_recorder_inputs();
    save_current_configuration(backend)
}
pub fn start_graph(backend: &Backend) -> Result<Vec<NodeStartResult>> {
    let _operation = backend.operation_lock.lock();
    stop_audition(backend);
    backend.start_graph_runtime()
}
pub fn stop_graph(backend: &Backend) {
    let _operation = backend.operation_lock.lock();
    backend.stop_graph_runtime();
    stop_audition(backend);
    let _ = save_current_configuration(backend);
}
pub fn recover_after_resume(backend: &Backend) -> Result<Vec<NodeStartResult>> {
    let _operation = backend.operation_lock.lock();
    backend.start_runtime()?;
    Ok(backend.nodes.recover_after_resume())
}
pub fn trigger_replay(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.trigger_replay(node_id)
}
pub fn pause_file(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.pause_file(node_id)
}
pub fn resume_file(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.resume_file(node_id)
}
pub fn seek_file(backend: &Backend, node_id: NodeId, seconds: f64) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.seek_file(node_id, seconds)
}
pub fn file_position(backend: &Backend, node_id: NodeId) -> Result<f64> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.file_position(node_id)
}

pub fn register_fixed_file_input(
    backend: &Backend,
    display_name: String,
    path: PathBuf,
    loop_enabled: bool,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    crate::recording::codecs::ensure_supported_extension(&path)?;
    if !path.is_file() {
        return Err(BackendError::InvalidOperation(format!(
            "audio file does not exist: {}",
            path.display()
        )));
    }
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::FileInput {
                path,
                looped: loop_enabled,
                loop_wait_ms: 0,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_edge_tab_input(
    backend: &Backend,
    display_name: String,
    stream_key: String,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    if stream_key.trim().is_empty() {
        return Err(BackendError::InvalidOperation(
            "Edge stream key cannot be empty".into(),
        ));
    }
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::EdgeTab {
                stream_key,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn get_edge_auth_token(backend: &Backend) -> String {
    let _operation = backend.operation_lock.lock();
    backend.edge.auth_token()
}

pub fn regenerate_edge_token(backend: &Backend) -> String {
    let _operation = backend.operation_lock.lock();
    backend.edge.regenerate_auth_token()
}

pub fn list_edge_tabs(backend: &Backend) -> Vec<EdgeTabSummary> {
    let _operation = backend.operation_lock.lock();
    backend.edge.list_tabs()
}

pub async fn control_edge_media(
    backend: &Backend,
    stream_key: String,
    action: EdgeMediaAction,
) -> Result<EdgeMediaResult> {
    backend.edge.control_media(&stream_key, action).await
}

pub fn register_process_pid_input(
    backend: &Backend,
    display_name: String,
    pid: u32,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    if pid == 0 {
        return Err(BackendError::InvalidOperation(
            "process PID must be greater than zero".into(),
        ));
    }
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::ProcessPid {
                pid,
                include_process_tree: true,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_process_exe_input(
    backend: &Backend,
    display_name: String,
    exe_path: PathBuf,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    if exe_path.as_os_str().is_empty() {
        return Err(BackendError::InvalidOperation(
            "process EXE path cannot be empty".into(),
        ));
    }
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::ProcessExe {
                exe_path,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_transform_plugin(
    backend: &Backend,
    request: TransformPluginCreateRequest,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let plugin_id = request.plugin_id.clone();
    let node = backend.plugins.build_node_spec(
        &request.plugin_id,
        request.display_name,
        request.parameters,
    )?;
    let mut definition = RegisteredNode::from_graph_node(&node);
    // Builtin DSP instances are freely placeable. Process plugins (including
    // RVC) retain the existing registered-instance placement limit.
    if backend.plugins.descriptor(&plugin_id)?.execution_mode == PluginExecutionMode::BuiltIn {
        definition.instance_policy = InstancePolicy::UnlimitedInstances;
    }
    definition.id = RegisteredNodeId::new();
    definition.origin = RegistrationOrigin::User;
    register_node_definition(backend, definition)
}

pub fn register_multi_input(backend: &Backend, display_name: String) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::MultiInput {
                slots: Vec::new(),
                legacy_ducking: None,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_test_signal(
    backend: &Backend,
    display_name: String,
    config: TestSignalConfig,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::TestSignal {
                config,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_input_selector(backend: &Backend, display_name: String) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::InputSelector {
                config: InputSelectorConfig::default(),
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_delay(
    backend: &Backend,
    display_name: String,
    config: DelayConfig,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::Delay {
                config,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_channel_mapper(
    backend: &Backend,
    display_name: String,
    config: ChannelMapperConfig,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::ChannelMapper {
                config,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn register_silence_detector(
    backend: &Backend,
    display_name: String,
    config: SilenceDetectorConfig,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    register_node_definition(
        backend,
        RegisteredNode::user(display_name, NodeProperties::SilenceDetector { config }),
    )
}

pub fn register_action_node(
    backend: &Backend,
    display_name: String,
    config: ActionNodeConfig,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    register_node_definition(
        backend,
        RegisteredNode::user(display_name, NodeProperties::Action { config }),
    )
}

pub fn register_log_output(
    backend: &Backend,
    display_name: String,
    config: LogOutputConfig,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    register_node_definition(
        backend,
        RegisteredNode::user(display_name, NodeProperties::LogOutput { config }),
    )
}

pub fn register_folder_input(
    backend: &Backend,
    display_name: String,
    path: PathBuf,
    mode: FolderReplayMode,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    crate::replay::folder_player::scan_folder(&path)?;
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::FolderReplay {
                path,
                mode,
                selected_file: None,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn create_replay_cache(backend: &Backend, config: ReplayCacheConfig) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    let config = backend.replay_caches.create_cache(config)?;
    let mut definition = RegisteredNode::user(
        config.display_name,
        NodeProperties::ReplayCache {
            cache_id: config.cache_id,
            gain_db: 0.0,
            mute: false,
        },
    );
    definition.enabled = config.enabled;
    let registered = register_node_definition(backend, definition)?;
    save_current_configuration(backend)?;
    Ok(registered)
}

pub fn list_replay_caches(backend: &Backend) -> Vec<ReplayCacheConfig> {
    let _operation = backend.operation_lock.lock();
    backend.replay_caches.list_caches()
}

pub fn register_memory_replay(
    backend: &Backend,
    display_name: String,
    cache_id: CacheId,
    mode: MemoryReplayMode,
    selected_item_id: Option<ReplayItemId>,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    backend.replay_caches.get_cache(cache_id)?;
    if let Some(item_id) = selected_item_id {
        backend.replay_caches.get_item(cache_id, item_id)?;
    }
    let mut definition = RegisteredNode::user(
        display_name,
        NodeProperties::MemoryReplay {
            cache_id,
            mode,
            selected_item_id,
            gain_db: 0.0,
            mute: false,
        },
    );
    definition.instance_policy = match mode {
        MemoryReplayMode::SelectFromCache => InstancePolicy::UnlimitedInstances,
        MemoryReplayMode::LatestFromCache => InstancePolicy::RegisteredInstanceOnly,
    };
    register_node_definition(backend, definition)
}

pub fn register_folder_recorder(
    backend: &Backend,
    display_name: String,
    directory: PathBuf,
    format: RecordingFormat,
    auto_record: bool,
) -> Result<RegisteredNode> {
    let _operation = backend.operation_lock.lock();
    std::fs::create_dir_all(&directory)?;
    if !directory.is_dir() {
        return Err(BackendError::InvalidOperation(format!(
            "recording directory is not a folder: {}",
            directory.display()
        )));
    }
    register_node_definition(
        backend,
        RegisteredNode::user(
            display_name,
            NodeProperties::Recorder {
                path: directory,
                format,
                directory_mode: true,
                auto_record,
                gain_db: 0.0,
                mute: false,
            },
        ),
    )
}

pub fn import_replay_cache_item(
    backend: &Backend,
    cache_id: CacheId,
    path: PathBuf,
    created_unix_ms: u64,
) -> Result<ReplayCacheItem> {
    let _operation = backend.operation_lock.lock();
    let item = backend
        .replay_caches
        .add_file(cache_id, path, created_unix_ms)?;
    save_current_configuration(backend)?;
    Ok(item)
}

pub fn list_memory_items(backend: &Backend, cache_id: CacheId) -> Result<Vec<ReplayCacheItem>> {
    let _operation = backend.operation_lock.lock();
    backend.replay_caches.list_items(cache_id)
}

pub fn play_replay(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.start_runtime()?;
    backend.nodes.play_replay(node_id)
}

pub fn pause_replay(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.pause_replay(node_id)
}

pub fn stop_replay(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.stop_replay(node_id)
}

pub fn seek_replay(backend: &Backend, node_id: NodeId, seconds: f64) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.seek_replay(node_id, seconds)
}

pub fn get_replay_state(backend: &Backend, node_id: NodeId) -> Result<ReplayStateSnapshot> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.replay_state(node_id)
}

pub fn list_folder_files(backend: &Backend, node_id: NodeId) -> Result<Vec<PathBuf>> {
    let _operation = backend.operation_lock.lock();
    ensure_replay_node_active(backend, node_id)?;
    backend.nodes.list_folder_files(node_id)
}

pub fn select_folder_file(backend: &Backend, node_id: NodeId, path: PathBuf) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    ensure_replay_node_active(backend, node_id)?;
    backend.nodes.select_folder_file(node_id, path.clone())?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::FolderReplay { selected_file, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not a Folder Replay".into(),
        ));
    };
    *selected_file = Some(path);
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_folder_replay_mode(
    backend: &Backend,
    node_id: NodeId,
    mode: FolderReplayMode,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    ensure_replay_node_active(backend, node_id)?;
    backend.nodes.set_folder_replay_mode(node_id, mode)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::FolderReplay { mode: stored, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not a Folder Replay".into(),
        ));
    };
    *stored = mode;
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn select_memory_item(
    backend: &Backend,
    node_id: NodeId,
    item_id: ReplayItemId,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::MemoryReplay {
        cache_id,
        mode,
        selected_item_id,
        ..
    } = &mut node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not a Memory Replay".into(),
        ));
    };
    if *mode == MemoryReplayMode::LatestFromCache {
        return Err(BackendError::InvalidOperation(
            "最新メモリーは項目を選択できません".into(),
        ));
    }
    backend.replay_caches.get_item(*cache_id, item_id)?;
    *mode = MemoryReplayMode::SelectFromCache;
    *selected_item_id = Some(item_id);
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn set_memory_cache(backend: &Backend, node_id: NodeId, cache_id: CacheId) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    backend.replay_caches.get_cache(cache_id)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::MemoryReplay {
        cache_id: stored_cache_id,
        selected_item_id,
        ..
    } = &mut node.properties
    else {
        return Err(BackendError::InvalidOperation(
            "node is not a Memory Replay".into(),
        ));
    };
    *stored_cache_id = cache_id;
    *selected_item_id = None;
    let was_active = backend.nodes.is_active(node_id);
    if was_active {
        backend.nodes.stop_node(node_id)?;
    }
    let node = backend.controller.update_node(node)?;
    if was_active && node.runtime_enabled {
        backend.nodes.start_node(node_id)?;
    }
    save_current_configuration(backend)?;
    Ok(node)
}

fn register_node_definition(backend: &Backend, node: RegisteredNode) -> Result<RegisteredNode> {
    let node = backend.registry.register(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

fn ensure_memory_replay_capacity(backend: &Backend, properties: &NodeProperties) -> Result<()> {
    if !matches!(
        properties,
        NodeProperties::MemoryReplay {
            mode: MemoryReplayMode::LatestFromCache,
            ..
        }
    ) {
        return Ok(());
    }
    if backend.controller.snapshot().nodes.values().any(|node| {
        matches!(
            &node.properties,
            NodeProperties::MemoryReplay {
                mode: MemoryReplayMode::LatestFromCache,
                ..
            }
        )
    }) {
        return Err(BackendError::InvalidOperation(
            "最新メモリーは1つだけ配置できます".into(),
        ));
    }
    Ok(())
}

fn ensure_replay_node_active(backend: &Backend, node_id: NodeId) -> Result<()> {
    if !backend.nodes.is_active(node_id) {
        backend.start_runtime()?;
        backend.nodes.start_node(node_id)?;
    }
    Ok(())
}

pub fn set_gain(backend: &Backend, node_id: NodeId, gain_db: f32) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    if !gain_db.is_finite() {
        return Err(BackendError::InvalidOperation("gain must be finite".into()));
    }
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    if !node.properties.set_gain_db(gain_db) {
        return Err(BackendError::InvalidOperation(
            "node has no master gain".into(),
        ));
    }
    let out = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(out)
}

pub fn set_mute(backend: &Backend, node_id: NodeId, mute: bool) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    if !node.properties.set_mute(mute) {
        return Err(BackendError::InvalidOperation(
            "node has no master mute".into(),
        ));
    }
    let out = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(out)
}

pub fn set_multi_input_slot(
    backend: &Backend,
    node_id: NodeId,
    port_id: crate::audio::graph::PortId,
    gain_db: f32,
    mute: bool,
) -> Result<NodeSpec> {
    let _operation = backend.operation_lock.lock();
    if let Some(edge_id) = backend
        .controller
        .snapshot()
        .edges
        .iter()
        .find(|edge| edge.to_node == node_id && edge.to_port == port_id)
        .map(|edge| edge.id)
    {
        backend
            .controller
            .set_edge_controls(edge_id, gain_db, mute)?;
        save_current_configuration(backend)?;
        return backend
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()));
    }
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::MultiInput { slots, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not Multi Input".into(),
        ));
    };
    let slot = slots
        .iter_mut()
        .find(|s| s.port_id == port_id)
        .ok_or_else(|| BackendError::PortNotFound(port_id.to_string()))?;
    slot.gain_db = gain_db;
    slot.mute = mute;
    let out = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(out)
}

fn update_node_properties(backend: &Backend, node: NodeSpec) -> Result<NodeSpec> {
    let node = backend.controller.update_node(node)?;
    if let Some(registered_node_id) = node.registered_node_id {
        let mut definition = backend.registry.get(registered_node_id)?;
        definition.properties = node.properties.clone();
        backend.registry.update(definition)?;
    }
    save_current_configuration(backend)?;
    Ok(node)
}

fn update_graph_node_properties(backend: &Backend, node: NodeSpec) -> Result<NodeSpec> {
    let node = backend.controller.update_node(node)?;
    save_current_configuration(backend)?;
    Ok(node)
}

pub fn get_test_signal_config(backend: &Backend, node_id: NodeId) -> Result<TestSignalConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::TestSignal { config, .. } => Ok(*config),
        _ => Err(BackendError::InvalidOperation(
            "node is not a test signal input".into(),
        )),
    }
}

pub fn update_test_signal(
    backend: &Backend,
    node_id: NodeId,
    config: TestSignalConfig,
) -> Result<TestSignalConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::TestSignal { config: stored, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not a test signal input".into(),
        ));
    };
    *stored = config;
    update_node_properties(backend, node)?;
    backend.nodes.update_test_signal(node_id, config)?;
    Ok(config)
}

pub fn get_input_selector_state(backend: &Backend, node_id: NodeId) -> Result<InputSelectorConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::InputSelector { config, .. } => Ok(config.clone()),
        _ => Err(BackendError::InvalidOperation(
            "node is not an input selector".into(),
        )),
    }
}

pub fn set_input_selector_config(
    backend: &Backend,
    node_id: NodeId,
    config: InputSelectorConfig,
) -> Result<InputSelectorConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    if config
        .inputs
        .iter()
        .any(|input| !node.input_ports.iter().any(|port| port.id == input.port_id))
    {
        return Err(BackendError::InvalidOperation(
            "存在しない入力Portの設定です".into(),
        ));
    }
    let NodeProperties::InputSelector { config: stored, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "スイッチャーではありません".into(),
        ));
    };
    *stored = config.clone();
    update_graph_node_properties(backend, node)?;
    Ok(config)
}
pub fn get_switcher_runtime(
    backend: &Backend,
    node_id: NodeId,
) -> crate::nodes::transform::input_selector::SwitcherSnapshot {
    backend.runtime.switcher_status(node_id)
}

fn connected_selector_ports(backend: &Backend, node_id: NodeId) -> Result<Vec<PortId>> {
    let graph = backend.controller.snapshot();
    let node = graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    if node.node_type != NodeType::InputSelector {
        return Err(BackendError::InvalidOperation(
            "node is not an input selector".into(),
        ));
    }
    Ok(node
        .input_ports
        .iter()
        .filter(|port| {
            graph
                .edges
                .iter()
                .any(|edge| edge.to_node == node_id && edge.to_port == port.id)
        })
        .map(|port| port.id)
        .collect())
}

pub fn set_input_selector_active_port(
    backend: &Backend,
    node_id: NodeId,
    active_port_id: Option<PortId>,
) -> Result<InputSelectorConfig> {
    let _operation = backend.operation_lock.lock();
    if let Some(port_id) = active_port_id {
        if !connected_selector_ports(backend, node_id)?.contains(&port_id) {
            return Err(BackendError::InvalidOperation(
                "selector input port is not connected".into(),
            ));
        }
    }
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::InputSelector { config, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not an input selector".into(),
        ));
    };
    config.active_port_id = active_port_id;
    config.selection_revision = config.selection_revision.wrapping_add(1);
    let result = config.clone();
    update_graph_node_properties(backend, node)?;
    Ok(result)
}

pub fn select_input_by_index(
    backend: &Backend,
    node_id: NodeId,
    index: usize,
) -> Result<InputSelectorConfig> {
    let _operation = backend.operation_lock.lock();
    let ports = connected_selector_ports(backend, node_id)?;
    let port_id = ports.get(index).copied().ok_or_else(|| {
        BackendError::InvalidOperation(format!("selector input index {index} is out of range"))
    })?;
    set_input_selector_active_port(backend, node_id, Some(port_id))
}

fn select_relative_input(
    backend: &Backend,
    node_id: NodeId,
    direction: isize,
) -> Result<InputSelectorConfig> {
    let ports = connected_selector_ports(backend, node_id)?;
    if ports.is_empty() {
        return Err(BackendError::InvalidOperation(
            "input selector has no connected inputs".into(),
        ));
    }
    let current = get_input_selector_state(backend, node_id)?.active_port_id;
    let index = current
        .and_then(|port_id| ports.iter().position(|candidate| *candidate == port_id))
        .unwrap_or(if direction < 0 { 0 } else { ports.len() - 1 });
    let next = (index as isize + direction).rem_euclid(ports.len() as isize) as usize;
    set_input_selector_active_port(backend, node_id, Some(ports[next]))
}

pub fn select_next_input(backend: &Backend, node_id: NodeId) -> Result<InputSelectorConfig> {
    let _operation = backend.operation_lock.lock();
    select_relative_input(backend, node_id, 1)
}

pub fn select_previous_input(backend: &Backend, node_id: NodeId) -> Result<InputSelectorConfig> {
    let _operation = backend.operation_lock.lock();
    select_relative_input(backend, node_id, -1)
}

pub fn get_delay_config(backend: &Backend, node_id: NodeId) -> Result<DelayConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::Delay { config, .. } => Ok(*config),
        _ => Err(BackendError::InvalidOperation("node is not a delay".into())),
    }
}

pub fn update_delay(
    backend: &Backend,
    node_id: NodeId,
    config: DelayConfig,
) -> Result<DelayConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::Delay { config: stored, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation("node is not a delay".into()));
    };
    *stored = config;
    update_node_properties(backend, node)?;
    Ok(config)
}

pub fn get_channel_mapper_config(
    backend: &Backend,
    node_id: NodeId,
) -> Result<ChannelMapperConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::ChannelMapper { config, .. } => Ok(config.clone()),
        _ => Err(BackendError::InvalidOperation(
            "node is not a channel mapper".into(),
        )),
    }
}

pub fn update_channel_mapper(
    backend: &Backend,
    node_id: NodeId,
    config: ChannelMapperConfig,
) -> Result<ChannelMapperConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::ChannelMapper { config: stored, .. } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not a channel mapper".into(),
        ));
    };
    *stored = config.clone();
    if node.input_ports.len() < 2 {
        let mut right = crate::audio::graph::PortSpec::input("入力2 / R");
        right.required = false;
        node.input_ports.push(right);
    }
    update_node_properties(backend, node)?;
    Ok(config)
}

pub fn get_silence_detector_config(
    backend: &Backend,
    node_id: NodeId,
) -> Result<SilenceDetectorConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::SilenceDetector { config } => Ok(*config),
        _ => Err(BackendError::InvalidOperation(
            "node is not a silence detector".into(),
        )),
    }
}

pub fn update_silence_detector(
    backend: &Backend,
    node_id: NodeId,
    config: SilenceDetectorConfig,
) -> Result<SilenceDetectorConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::SilenceDetector { config: stored } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not a silence detector".into(),
        ));
    };
    *stored = config;
    update_node_properties(backend, node)?;
    Ok(config)
}

pub fn get_silence_detector_state(
    backend: &Backend,
    node_id: NodeId,
) -> Result<crate::nodes::transform::silence_detector::SilenceDetectorStateSnapshot> {
    let _operation = backend.operation_lock.lock();
    backend.nodes.silence_detector_state(node_id)
}

pub fn get_action_config(backend: &Backend, node_id: NodeId) -> Result<ActionNodeConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::Action { config } => Ok(config.clone()),
        _ => Err(BackendError::InvalidOperation(
            "node is not an Action node".into(),
        )),
    }
}

pub fn update_action_node(
    backend: &Backend,
    node_id: NodeId,
    config: ActionNodeConfig,
) -> Result<ActionNodeConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    if let crate::actions::model::ActionConfig::Obs(obs) = &config.action {
        if !backend
            .action_engine
            .obs_connections()
            .contains_key(&obs.connection_id)
        {
            return Err(BackendError::InvalidOperation(
                "OBS connection is not configured".into(),
            ));
        }
    }
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::Action { config: stored } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not an Action node".into(),
        ));
    };
    *stored = config.clone();
    update_node_properties(backend, node)?;
    Ok(config)
}

pub fn test_action_node(backend: &Backend, node_id: NodeId) -> Result<ActionExecutionId> {
    let _operation = backend.operation_lock.lock();
    backend.action_engine.execute_test(node_id)
}

pub fn get_action_runtime_state(backend: &Backend, node_id: NodeId) -> Result<ActionRuntimeState> {
    let _operation = backend.operation_lock.lock();
    get_action_config(backend, node_id)?;
    Ok(backend.action_engine.runtime_state(node_id))
}

pub fn get_log_output_config(backend: &Backend, node_id: NodeId) -> Result<LogOutputConfig> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    match &graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?
        .properties
    {
        NodeProperties::LogOutput { config } => Ok(config.clone()),
        _ => Err(BackendError::InvalidOperation(
            "node is not a Log Output node".into(),
        )),
    }
}

pub fn update_log_output(
    backend: &Backend,
    node_id: NodeId,
    config: LogOutputConfig,
) -> Result<LogOutputConfig> {
    let _operation = backend.operation_lock.lock();
    config.validate().map_err(BackendError::InvalidOperation)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    let NodeProperties::LogOutput { config: stored } = &mut node.properties else {
        return Err(BackendError::InvalidOperation(
            "node is not a Log Output node".into(),
        ));
    };
    *stored = config.clone();
    update_node_properties(backend, node)?;
    Ok(config)
}

pub fn list_obs_connections(backend: &Backend) -> Vec<ObsConnectionConfig> {
    let _operation = backend.operation_lock.lock();
    backend.action_engine.list_obs_connections()
}

pub fn upsert_obs_connection(
    backend: &Backend,
    config: ObsConnectionConfig,
) -> Result<ObsConnectionConfig> {
    let _operation = backend.operation_lock.lock();
    let config = backend.action_engine.upsert_obs_connection(config)?;
    save_current_configuration(backend)?;
    Ok(config)
}

pub fn delete_obs_connection(backend: &Backend, id: ObsConnectionId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.action_engine.delete_obs_connection(id)?;
    save_current_configuration(backend)
}

pub fn set_action_secret(
    backend: &Backend,
    id: Option<SecretId>,
    value: String,
) -> Result<SecretId> {
    let _operation = backend.operation_lock.lock();
    if value.is_empty() {
        return Err(BackendError::InvalidOperation(
            "secret value must not be empty".into(),
        ));
    }
    backend.action_engine.set_secret_persistent(id, value)
}

pub fn delete_action_secret(backend: &Backend, id: SecretId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    backend.action_engine.delete_secret_persistent(id)
}

pub fn set_obs_connection_password(
    backend: &Backend,
    connection_id: ObsConnectionId,
    password: String,
) -> Result<ObsConnectionConfig> {
    let _operation = backend.operation_lock.lock();
    let mut connection = backend
        .action_engine
        .obs_connections()
        .get(&connection_id)
        .cloned()
        .ok_or_else(|| BackendError::InvalidOperation("OBS connection not found".into()))?;
    let secret_id = backend
        .action_engine
        .set_secret_persistent(connection.password_secret_id, password)?;
    connection.password_secret_id = Some(secret_id);
    let connection = backend.action_engine.upsert_obs_connection(connection)?;
    save_current_configuration(backend)?;
    Ok(connection)
}

pub fn list_node_actions(backend: &Backend, node_id: NodeId) -> Result<Vec<NodeActionDescriptor>> {
    let _operation = backend.operation_lock.lock();
    NodeActionDispatcher::list_actions(backend, node_id)
}

pub fn list_shortcuts(backend: &Backend) -> Vec<ShortcutBinding> {
    let _operation = backend.operation_lock.lock();
    backend.shortcut_engine.list()
}

pub fn validate_shortcut(
    backend: &Backend,
    binding: ShortcutBinding,
) -> Result<ShortcutValidation> {
    let _operation = backend.operation_lock.lock();
    NodeActionDispatcher::validate(
        backend,
        binding.node_id,
        &binding.action_id,
        binding.action_parameter.as_ref(),
    )?;
    Ok(backend.shortcut_engine.validate(&binding))
}

pub fn create_shortcut(
    backend: &Backend,
    request: CreateShortcutRequest,
) -> Result<ShortcutBinding> {
    let _operation = backend.operation_lock.lock();
    let binding = ShortcutBinding {
        id: ShortcutId::new(),
        enabled: request.enabled,
        key_combo: request.key_combo,
        node_id: request.node_id,
        action_id: request.action_id,
        action_parameter: request.action_parameter,
    };
    NodeActionDispatcher::validate(
        backend,
        binding.node_id,
        &binding.action_id,
        binding.action_parameter.as_ref(),
    )?;
    let binding = backend.shortcut_engine.create(binding)?;
    if let Err(error) = save_current_configuration(backend) {
        let _ = backend.shortcut_engine.delete(binding.id);
        return Err(error);
    }
    Ok(binding)
}

pub fn update_shortcut(backend: &Backend, binding: ShortcutBinding) -> Result<ShortcutBinding> {
    let _operation = backend.operation_lock.lock();
    NodeActionDispatcher::validate(
        backend,
        binding.node_id,
        &binding.action_id,
        binding.action_parameter.as_ref(),
    )?;
    let previous = backend
        .shortcut_engine
        .get(binding.id)
        .ok_or_else(|| BackendError::InvalidOperation("shortcut binding not found".into()))?;
    let binding = backend.shortcut_engine.update(binding)?;
    if let Err(error) = save_current_configuration(backend) {
        let _ = backend.shortcut_engine.update(previous);
        return Err(error);
    }
    Ok(binding)
}

pub fn delete_shortcut(backend: &Backend, id: ShortcutId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let previous = backend.shortcut_engine.delete(id)?;
    if let Err(error) = save_current_configuration(backend) {
        let _ = backend.shortcut_engine.create(previous);
        return Err(error);
    }
    Ok(())
}

pub fn set_shortcut_enabled(
    backend: &Backend,
    id: ShortcutId,
    enabled: bool,
) -> Result<ShortcutBinding> {
    let _operation = backend.operation_lock.lock();
    let previous = backend
        .shortcut_engine
        .get(id)
        .ok_or_else(|| BackendError::InvalidOperation("shortcut binding not found".into()))?;
    let binding = backend.shortcut_engine.set_enabled(id, enabled)?;
    if let Err(error) = save_current_configuration(backend) {
        let _ = backend.shortcut_engine.set_enabled(id, previous.enabled);
        return Err(error);
    }
    Ok(binding)
}

pub fn list_volume_profiles(backend: &Backend) -> Result<Vec<VolumeMuteProfile>> {
    let _operation = backend.operation_lock.lock();
    let mut profiles: Vec<_> = backend
        .config
        .load()?
        .volume_profiles
        .into_values()
        .collect();
    profiles.sort_by(|left, right| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left.profile_id.cmp(&right.profile_id))
    });
    Ok(profiles)
}

pub fn save_volume_profile(
    backend: &Backend,
    profile_id: Option<VolumeProfileId>,
    display_name: String,
) -> Result<VolumeMuteProfile> {
    let _operation = backend.operation_lock.lock();
    let mut persisted = backend.config.load()?;
    let now = unix_time_ms();
    let (profile_id, created_unix_ms) = match profile_id {
        Some(profile_id) => {
            let existing = persisted.volume_profiles.get(&profile_id).ok_or_else(|| {
                BackendError::InvalidOperation(format!("volume profile {profile_id} not found"))
            })?;
            (profile_id, existing.created_unix_ms)
        }
        None => (VolumeProfileId::new(), now),
    };
    let profile = VolumeMuteProfile::capture(
        &backend.controller.snapshot(),
        profile_id,
        display_name,
        created_unix_ms,
        now,
    )?;
    persisted
        .volume_profiles
        .insert(profile.profile_id, profile.clone());
    backend.config.save(&persisted)?;
    Ok(profile)
}

pub fn apply_volume_profile(
    backend: &Backend,
    profile_id: VolumeProfileId,
) -> Result<ApplyVolumeProfileReport> {
    let _operation = backend.operation_lock.lock();
    let profile = backend
        .config
        .load()?
        .volume_profiles
        .get(&profile_id)
        .cloned()
        .ok_or_else(|| {
            BackendError::InvalidOperation(format!("volume profile {profile_id} not found"))
        })?;
    let mut graph = backend.controller.snapshot();
    let mut report = profile.apply_to(&mut graph)?;
    backend.controller.replace_graph(graph)?;
    report.replays_stopped = backend.nodes.stop_all_replays();
    save_current_configuration(backend)?;
    Ok(report)
}

pub fn delete_volume_profile(backend: &Backend, profile_id: VolumeProfileId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let mut persisted = backend.config.load()?;
    if persisted.volume_profiles.remove(&profile_id).is_none() {
        return Err(BackendError::InvalidOperation(format!(
            "volume profile {profile_id} not found"
        )));
    }
    backend.config.save(&persisted)
}

pub fn start_recording(backend: &Backend, node_id: NodeId) -> Result<NodeStartResult> {
    let _operation = backend.operation_lock.lock();
    let snapshot = start_recorder(backend, node_id)?;
    Ok(NodeStartResult {
        node_id,
        state: recorder_runtime_state(snapshot.state),
        message: None,
    })
}

pub fn start_recorder(backend: &Backend, node_id: NodeId) -> Result<RecorderStateSnapshot> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node_id)?;
    ensure_recorder(backend, node_id)?;
    backend.start_runtime()?;
    backend.nodes.start_recorder(node_id)
}

pub fn stop_recording(backend: &Backend, node_id: NodeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    stop_recorder(backend, node_id).map(|_| ())
}

pub fn stop_recorder(backend: &Backend, node_id: NodeId) -> Result<Option<RecordingOutput>> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node_id)?;
    ensure_recorder(backend, node_id)?;
    let output = backend.nodes.stop_recorder(node_id)?;
    save_current_configuration(backend)?;
    Ok(output)
}

pub fn set_auto_record(
    backend: &Backend,
    node_id: NodeId,
    auto_record: bool,
) -> Result<RecorderStateSnapshot> {
    let _operation = backend.operation_lock.lock();
    ensure_output_operation_allowed(backend, node_id)?;
    let mut node = backend
        .controller
        .snapshot()
        .nodes
        .get(&node_id)
        .cloned()
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    match &mut node.properties {
        NodeProperties::Recorder {
            auto_record: stored,
            ..
        } => {
            *stored = auto_record;
            node = backend.controller.update_node(node)?;
            if let Some(registered_node_id) = node.registered_node_id {
                let mut registered = backend.registry.get(registered_node_id)?;
                registered.properties = node.properties.clone();
                backend.registry.update(registered)?;
            }
        }
        NodeProperties::ReplayCache { cache_id, .. } => {
            let mut cache = backend.replay_caches.get_cache(*cache_id)?;
            cache.auto_record = auto_record;
            backend.replay_caches.update_cache(cache)?;
        }
        _ => {
            return Err(BackendError::InvalidOperation(
                "node is not a recording output".into(),
            ))
        }
    }
    backend
        .nodes
        .set_recorder_auto_record(node_id, auto_record)?;
    save_current_configuration(backend)?;
    backend.nodes.recorder_state(node_id)
}

pub fn get_recorder_state(backend: &Backend, node_id: NodeId) -> Result<RecorderStateSnapshot> {
    let _operation = backend.operation_lock.lock();
    ensure_recorder(backend, node_id)?;
    backend.nodes.recorder_state(node_id)
}

fn ensure_recorder(backend: &Backend, node_id: NodeId) -> Result<()> {
    let graph = backend.controller.snapshot();
    let node = graph
        .nodes
        .get(&node_id)
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
    matches!(
        &node.properties,
        NodeProperties::Recorder { .. } | NodeProperties::ReplayCache { .. }
    )
    .then_some(())
    .ok_or_else(|| BackendError::InvalidOperation("node is not a recording output".into()))
}

fn recorder_runtime_state(state: RecorderState) -> RuntimeState {
    match state {
        RecorderState::Disabled => RuntimeState::Stopped,
        RecorderState::Ready | RecorderState::Recording => RuntimeState::Running,
        RecorderState::WaitingForInput => RuntimeState::Waiting,
        RecorderState::Stopping => RuntimeState::Recovering,
        RecorderState::Error => RuntimeState::Error,
    }
}

pub fn list_recordings(backend: &Backend) -> Vec<RecordingInfo> {
    let _operation = backend.operation_lock.lock();
    let mut recordings = Vec::new();
    for node in backend.controller.snapshot().nodes.values() {
        match &node.properties {
            NodeProperties::Recorder {
                path,
                directory_mode,
                ..
            } if *directory_mode => {
                let Ok(entries) = std::fs::read_dir(path) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let recording_path = entry.path();
                    if !recording_path.is_file()
                        || RecordingFormat::from_path(&recording_path).is_none()
                    {
                        continue;
                    }
                    let meta = entry.metadata().ok();
                    recordings.push(RecordingInfo {
                        node_id: node.id,
                        path: recording_path,
                        exists: meta.is_some(),
                        bytes: meta.map(|value| value.len()),
                    });
                }
            }
            NodeProperties::Recorder { path, .. } => {
                let meta = std::fs::metadata(path).ok();
                recordings.push(RecordingInfo {
                    node_id: node.id,
                    path: path.clone(),
                    exists: meta.is_some(),
                    bytes: meta.map(|value| value.len()),
                });
            }
            NodeProperties::ReplayCache { cache_id, .. } => {
                if let Ok(items) = backend.replay_caches.list_items(*cache_id) {
                    recordings.extend(items.into_iter().map(|item| {
                        let meta = std::fs::metadata(&item.path).ok();
                        RecordingInfo {
                            node_id: node.id,
                            path: item.path,
                            exists: meta.is_some(),
                            bytes: meta.map(|value| value.len()),
                        }
                    }));
                }
            }
            _ => {}
        }
    }
    recordings.sort_by(|a, b| a.path.cmp(&b.path));
    recordings
}

pub fn get_waveform_command(path: impl AsRef<Path>, buckets: usize) -> Result<Vec<WaveformBucket>> {
    get_waveform(path, buckets)
}
pub fn get_cached_waveform_command(
    backend: &Backend,
    path: impl AsRef<Path>,
    buckets: usize,
) -> Result<Vec<WaveformBucket>> {
    let _operation = backend.operation_lock.lock();
    get_cached_waveform(
        path,
        buckets,
        backend.config.base_dir().join("audio-analysis"),
    )
}
pub fn trim_recording(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    range: TrimRange,
) -> Result<()> {
    export_trimmed(input, output, range)
}

pub fn list_rvc_instances(backend: &Backend) -> Vec<RvcInstanceId> {
    let _operation = backend.operation_lock.lock();
    backend.rvc.list()
}

pub fn configure_rvc(backend: &Backend, config: RvcConfiguration) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let concrete = PassthroughRvcBackend::new(config.passthrough_latency_frames);
    let processor = RvcNodeProcessor::new(Box::new(concrete));
    processor.initialize()?;
    backend.rvc.register(config.instance_id, processor);
    Ok(())
}

pub fn enable_rvc(backend: &Backend, instance_id: RvcInstanceId, enabled: bool) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let ids: Vec<NodeId> = graph.nodes.values()
        .filter(|n| matches!(&n.properties, NodeProperties::Rvc { instance_id: id, .. } if *id == instance_id))
        .map(|n| n.id)
        .collect();
    if ids.is_empty() {
        return Err(BackendError::Rvc(format!(
            "RVC instance {} has no graph node",
            instance_id
        )));
    }
    for id in ids {
        let mut node = backend
            .controller
            .snapshot()
            .nodes
            .get(&id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(id.to_string()))?;
        node.enabled = enabled;
        backend.controller.update_node(node)?;
        if enabled {
            backend.start_runtime()?;
            if !backend.nodes.is_active(id) {
                backend.nodes.start_node(id)?;
            }
        } else if backend.nodes.is_active(id) {
            backend.nodes.stop_node(id)?;
        }
    }
    Ok(())
}

pub fn bypass_rvc(backend: &Backend, instance_id: RvcInstanceId, bypass: bool) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    if let Ok(processor) = backend.rvc.get(instance_id) {
        processor.set_bypass(bypass)?;
    }
    let graph = backend.controller.snapshot();
    let mut found = false;
    for mut node in graph.nodes.values().filter(|n| matches!(&n.properties, NodeProperties::Rvc { instance_id: id, .. } if *id == instance_id)).cloned() {
        if let NodeProperties::Rvc { bypass: flag, .. } = &mut node.properties { *flag = bypass; }
        found = true;
        backend.controller.update_node(node)?;
    }
    if !found {
        return Err(BackendError::Rvc(format!(
            "RVC instance {} has no graph node",
            instance_id
        )));
    }
    Ok(())
}

pub fn rvc_state(backend: &Backend, instance_id: RvcInstanceId) -> Result<RvcState> {
    let _operation = backend.operation_lock.lock();
    Ok(backend.rvc.get(instance_id)?.state())
}

pub fn list_virtual_device_pairs(backend: &Backend) -> Result<Vec<VirtualDevicePair>> {
    let _operation = backend.operation_lock.lock();
    Ok(backend.config.load()?.virtual_device_pairs)
}

pub fn upsert_virtual_device_pair(
    backend: &Backend,
    pair: VirtualDevicePair,
) -> Result<VirtualDevicePair> {
    let _operation = backend.operation_lock.lock();
    pair.validate()?;
    let mut config = backend.config.load()?;
    if config.virtual_device_pairs.iter().any(|x| {
        x.id != pair.id
            && (x.render_endpoint_id == pair.render_endpoint_id
                || x.capture_endpoint_id == pair.capture_endpoint_id)
    }) {
        return Err(BackendError::InvalidOperation(
            "the render or capture endpoint is already assigned to another virtual-device pair"
                .into(),
        ));
    }
    let previous_pair = config
        .virtual_device_pairs
        .iter()
        .find(|x| x.id == pair.id)
        .cloned();
    if let Some(existing) = config
        .virtual_device_pairs
        .iter_mut()
        .find(|x| x.id == pair.id)
    {
        *existing = pair.clone();
    } else {
        config.virtual_device_pairs.push(pair.clone());
    }
    let mut registered_nodes = backend.registry.snapshot();
    if let Some(previous_pair) = previous_pair {
        set_virtual_device_pair_registration_flags(&mut registered_nodes, &previous_pair, false);
    }
    set_virtual_device_pair_registration_flags(&mut registered_nodes, &pair, true);
    config.registered_nodes = registered_nodes.clone();
    persist_configuration_candidate(backend, config)?;
    backend.registry.replace_all(registered_nodes);
    Ok(pair)
}

pub fn delete_virtual_device_pair(backend: &Backend, id: &str) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let mut config = backend.config.load()?;
    let pair = config
        .virtual_device_pairs
        .iter()
        .find(|pair| pair.id == id)
        .cloned();
    let old_len = config.virtual_device_pairs.len();
    config.virtual_device_pairs.retain(|x| x.id != id);
    if config.virtual_device_pairs.len() == old_len {
        return Err(BackendError::InvalidOperation(format!(
            "virtual device pair '{id}' not found"
        )));
    }
    let mut registered_nodes = backend.registry.snapshot();
    if let Some(pair) = pair {
        set_virtual_device_pair_registration_flags(&mut registered_nodes, &pair, false);
    }
    config.registered_nodes = registered_nodes.clone();
    persist_configuration_candidate(backend, config)?;
    backend.registry.replace_all(registered_nodes);
    Ok(())
}

fn set_virtual_device_pair_registration_flags(
    registered_nodes: &mut HashMap<RegisteredNodeId, RegisteredNode>,
    pair: &VirtualDevicePair,
    is_virtual: bool,
) {
    for registered in registered_nodes.values_mut() {
        let matches_pair_endpoint = match registered.node_type {
            NodeType::Monitor => {
                registered.resource_key.as_deref() == Some(pair.render_endpoint_id.as_str())
            }
            NodeType::Microphone => {
                registered.resource_key.as_deref() == Some(pair.capture_endpoint_id.as_str())
            }
            _ => false,
        };
        if matches_pair_endpoint {
            registered.is_virtual = is_virtual;
        }
    }
}

pub fn detect_listen_route(capture_endpoint_id: &str) -> ListenDetection {
    detect_listen_to_device(capture_endpoint_id)
}

pub fn list_implicit_routes_command(backend: &Backend) -> Result<Vec<ImplicitRoute>> {
    let _operation = backend.operation_lock.lock();
    let config = backend.config.load()?;
    Ok(list_implicit_routes(&config.virtual_device_pairs))
}

pub fn get_loop_warnings(backend: &Backend) -> Vec<LoopWarning> {
    let _operation = backend.operation_lock.lock();
    let routes = backend
        .config
        .load()
        .ok()
        .map(|c| list_implicit_routes(&c.virtual_device_pairs))
        .unwrap_or_default();
    crate::routing::loop_detector::detect_loop_warnings_with_implicit(
        &backend.controller.snapshot(),
        &routes,
    )
}

pub fn get_latency_report(backend: &Backend) -> Vec<PathLatencyReport> {
    let _operation = backend.operation_lock.lock();
    backend.runtime.latency_report()
}

#[derive(Clone)]
struct FullProfileState {
    graph: GraphSnapshot,
    registered_nodes: HashMap<RegisteredNodeId, RegisteredNode>,
    obs_connections: HashMap<ObsConnectionId, ObsConnectionConfig>,
    shortcuts: HashMap<ShortcutId, ShortcutBinding>,
    replay_cache_metadata: ReplayCacheMetadata,
    active_profile: Option<String>,
}

fn capture_full_profile_state(backend: &Backend) -> FullProfileState {
    FullProfileState {
        graph: backend.controller.snapshot(),
        registered_nodes: backend.registry.snapshot(),
        obs_connections: backend.action_engine.obs_connections(),
        shortcuts: backend.shortcut_engine.snapshot(),
        replay_cache_metadata: backend.replay_caches.snapshot(),
        active_profile: backend.active_routing_profile.lock().clone(),
    }
}

fn validate_full_profile(profile: &RoutingProfile) -> Result<()> {
    let staging = crate::controller::GraphController::new();
    let graph = profile.graph();
    staging.replace_graph(graph.clone())?;
    for connection in profile.obs_connections.values() {
        connection
            .validate()
            .map_err(BackendError::InvalidOperation)?;
    }
    for (cache_id, cache) in &profile.replay_cache_metadata.caches {
        if cache.cache_id != *cache_id || matches!(cache.max_items, Some(0)) {
            return Err(BackendError::InvalidOperation(
                "Full ProfileのReplay Cache設定が不正です".into(),
            ));
        }
    }
    if profile.replay_cache_metadata.items.values().any(|item| {
        !profile
            .replay_cache_metadata
            .caches
            .contains_key(&item.cache_id)
    }) {
        return Err(BackendError::InvalidOperation(
            "Full ProfileのReplay項目が存在しないCacheを参照しています".into(),
        ));
    }

    let reserved = default_reserved_combinations();
    let mut enabled_combinations = HashMap::new();
    for (shortcut_id, binding) in &profile.shortcuts {
        if binding.id != *shortcut_id {
            return Err(BackendError::InvalidOperation(
                "Full ProfileのShortcut IDが一致しません".into(),
            ));
        }
        binding
            .key_combo
            .validate()
            .map_err(BackendError::InvalidOperation)?;
        if reserved.contains(&binding.key_combo) {
            return Err(BackendError::InvalidOperation(
                "Full ProfileにVoiceMixer予約Shortcutが含まれています".into(),
            ));
        }
        let node = graph.nodes.get(&binding.node_id).cloned().ok_or_else(|| {
            BackendError::InvalidOperation(
                "Full ProfileのShortcutが存在しないNodeを参照しています".into(),
            )
        })?;
        NodeActionDispatcher::validate_node_action(
            &node,
            &binding.action_id,
            binding.action_parameter.as_ref(),
        )?;
        if binding.enabled
            && enabled_combinations
                .insert(binding.key_combo, binding.id)
                .is_some()
        {
            return Err(BackendError::InvalidOperation(
                "Full Profileに重複Shortcutが含まれています".into(),
            ));
        }
    }
    Ok(())
}

fn profile_from_backend(
    backend: &Backend,
    profile_id: String,
    display_name: String,
    description: String,
) -> RoutingProfile {
    let graph = backend.controller.snapshot();
    RoutingProfile {
        version: ROUTING_PROFILE_SCHEMA_VERSION,
        profile_id,
        display_name,
        description,
        nodes: graph
            .nodes
            .into_iter()
            .map(|(id, mut node)| {
                node.state = RuntimeState::Stopped;
                (id, node)
            })
            .collect(),
        routing_edges: graph.edges,
        event_edges: graph.event_edges,
        node_layouts: graph.layouts,
        registered_nodes: backend.registry.snapshot(),
        obs_connections: backend.action_engine.obs_connections(),
        shortcuts: backend.shortcut_engine.snapshot(),
        replay_cache_metadata: backend.replay_caches.snapshot(),
    }
}

fn apply_full_profile_state(backend: &Backend, mut state: FullProfileState) -> Result<()> {
    // Profiles and the current configuration can contain the old default name
    // in both registered definitions and placed graph instances. Migrate only
    // Replay Cache nodes; arbitrary user nodes named "Replay Cache" are kept.
    for node in state.registered_nodes.values_mut() {
        if node.node_type == NodeType::ReplayCache {
            migrate_legacy_replay_cache_name(&mut node.display_name);
        }
    }
    for node in state.graph.nodes.values_mut() {
        if node.node_type == NodeType::ReplayCache {
            migrate_legacy_replay_cache_name(&mut node.display_name);
        }
    }
    backend.controller.replace_graph(state.graph)?;
    backend.registry.replace_all(state.registered_nodes);
    backend
        .action_engine
        .replace_obs_connections(state.obs_connections)?;
    backend.shortcut_engine.replace_bindings(state.shortcuts)?;
    backend.replay_caches.replace(state.replay_cache_metadata);
    *backend.active_routing_profile.lock() = state.active_profile;
    Ok(())
}

pub fn list_routing_profiles(backend: &Backend) -> Result<Vec<RoutingProfileSummary>> {
    let _operation = backend.operation_lock.lock();
    routing_profile::list(&backend.config)
}

pub fn load_routing_profile(
    backend: &Backend,
    profile_id: String,
) -> Result<RoutingProfileSummary> {
    let _operation = backend.operation_lock.lock();
    let profile = routing_profile::load(&backend.config, &profile_id)?;
    validate_full_profile(&profile)?;
    // 次のProfileを適用して現在のGraphを失う前に、現在選択中のユーザー
    // Projectと作業状態を同じSnapshotで保存する。
    save_current_configuration(backend)?;
    let summary = profile.summary(format!("{}.json", profile.profile_id));
    let previous = capture_full_profile_state(backend);
    let next = FullProfileState {
        graph: profile.graph(),
        registered_nodes: if profile.version >= ROUTING_PROFILE_SCHEMA_VERSION {
            profile.registered_nodes.clone()
        } else {
            previous.registered_nodes.clone()
        },
        obs_connections: if profile.version >= ROUTING_PROFILE_SCHEMA_VERSION {
            profile.obs_connections.clone()
        } else {
            previous.obs_connections.clone()
        },
        shortcuts: if profile.version >= ROUTING_PROFILE_SCHEMA_VERSION {
            profile.shortcuts.clone()
        } else {
            previous.shortcuts.clone()
        },
        replay_cache_metadata: if profile.version >= ROUTING_PROFILE_SCHEMA_VERSION {
            profile.replay_cache_metadata.clone()
        } else {
            previous.replay_cache_metadata.clone()
        },
        active_profile: Some(profile.profile_id.clone()),
    };

    backend.input_monitor.stop_all();
    backend.stop_graph_runtime();
    stop_audition(backend);
    if let Err(error) = apply_full_profile_state(backend, next) {
        return Err(restore_routing_profile(backend, previous, error));
    }
    if let Err(error) = save_current_configuration(backend) {
        return Err(restore_routing_profile(backend, previous, error));
    }
    if let Err(error) = backend.start_graph_runtime() {
        return Err(restore_routing_profile(backend, previous, error));
    }
    Ok(summary)
}

pub fn save_routing_project_as(
    backend: &Backend,
    display_name: String,
) -> Result<RoutingProfileSummary> {
    let _operation = backend.operation_lock.lock();
    let display_name = display_name.trim();
    if display_name.is_empty() {
        return Err(BackendError::InvalidOperation(
            "プロジェクト名を入力してください".into(),
        ));
    }
    let profile_id = format!("project-{}", uuid::Uuid::new_v4());
    let profile = profile_from_backend(
        backend,
        profile_id.clone(),
        display_name.to_owned(),
        "ユーザーが保存したFull Profileです。".into(),
    );
    let summary = routing_profile::save(&backend.config, &profile)?;
    let previous_active = backend.active_routing_profile.lock().clone();
    *backend.active_routing_profile.lock() = Some(profile_id);
    if let Err(error) = save_current_configuration(backend) {
        *backend.active_routing_profile.lock() = previous_active;
        return Err(error);
    }
    Ok(summary)
}

pub fn save_routing_project(backend: &Backend) -> Result<RoutingProfileSummary> {
    let _operation = backend.operation_lock.lock();
    let profile_id = backend
        .active_routing_profile
        .lock()
        .clone()
        .ok_or_else(|| BackendError::InvalidOperation("保存先のFull Profileがありません".into()))?;
    if matches!(
        profile_id.as_str(),
        routing_profile::SAMPLE_MONITOR_PROFILE_ID
            | routing_profile::SAMPLE_RECORDING_PROFILE_ID
            | routing_profile::SAMPLE_BROADCAST_PROFILE_ID
            | routing_profile::SAMPLE_BRIDGE_CAST_MIX_PROFILE_ID
    ) {
        return Err(BackendError::InvalidOperation(
            "組み込みSampleは別名で保存してください".into(),
        ));
    }
    let current = routing_profile::load(&backend.config, &profile_id)?;
    let profile = profile_from_backend(
        backend,
        profile_id,
        current.display_name,
        current.description,
    );
    let summary = routing_profile::save(&backend.config, &profile)?;
    save_current_configuration(backend)?;
    Ok(summary)
}

pub fn new_routing_project(backend: &Backend) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    // 新規Projectへ切り替える場合も、編集中Projectを先に永続化する。
    save_current_configuration(backend)?;
    let previous = capture_full_profile_state(backend);
    let next = FullProfileState {
        graph: GraphSnapshot::default(),
        registered_nodes: HashMap::new(),
        obs_connections: HashMap::new(),
        shortcuts: HashMap::new(),
        replay_cache_metadata: ReplayCacheMetadata::default(),
        active_profile: None,
    };
    backend.input_monitor.stop_all();
    backend.stop_graph_runtime();
    if let Err(error) = apply_full_profile_state(backend, next) {
        return Err(restore_routing_profile(backend, previous, error));
    }
    if let Err(error) = save_current_configuration(backend) {
        return Err(restore_routing_profile(backend, previous, error));
    }
    if let Err(error) = backend.start_graph_runtime() {
        return Err(restore_routing_profile(backend, previous, error));
    }
    Ok(())
}

pub fn delete_routing_project(backend: &Backend, profile_id: String) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    if !routing_profile::is_user_project(&profile_id) {
        return Err(BackendError::InvalidOperation(
            "組み込みサンプルは削除できません".into(),
        ));
    }
    routing_profile::load(&backend.config, &profile_id)?;
    let previous_active = backend.active_routing_profile.lock().clone();
    let was_active = previous_active.as_deref() == Some(profile_id.as_str());
    if was_active {
        // 音声処理と編集中のGraphは保持し、保存先だけを外す。
        *backend.active_routing_profile.lock() = None;
        if let Err(error) = save_current_configuration(backend) {
            *backend.active_routing_profile.lock() = previous_active;
            return Err(error);
        }
    }
    if let Err(error) = routing_profile::delete(&backend.config, &profile_id) {
        if was_active {
            *backend.active_routing_profile.lock() = previous_active;
            save_current_configuration(backend)?;
        }
        return Err(error);
    }
    Ok(())
}

fn restore_routing_profile(
    backend: &Backend,
    previous: FullProfileState,
    original_error: BackendError,
) -> BackendError {
    backend.stop_graph_runtime();
    let mut rollback_errors = Vec::new();
    if let Err(error) = apply_full_profile_state(backend, previous) {
        rollback_errors.push(format!("full profile restore: {error}"));
    }
    if let Err(error) = save_current_configuration(backend) {
        rollback_errors.push(format!("configuration restore: {error}"));
    }
    if let Err(error) = backend.start_graph_runtime() {
        rollback_errors.push(format!("runtime restore: {error}"));
    }
    if rollback_errors.is_empty() {
        original_error
    } else {
        BackendError::InvalidOperation(format!(
            "{original_error}; routing profile rollback failed: {}",
            rollback_errors.join("; ")
        ))
    }
}

pub fn save_current_configuration(backend: &Backend) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    save_graph_configuration(backend, graph)
}

fn save_graph_configuration(backend: &Backend, graph: GraphSnapshot) -> Result<()> {
    let mut persisted: PersistedConfiguration = backend.config.load()?;
    persisted.nodes = graph
        .nodes
        .into_iter()
        .map(|(id, mut node)| {
            node.state = crate::audio::graph::RuntimeState::Stopped;
            (id, node)
        })
        .collect();
    persisted.routing_edges = graph.edges;
    persisted.event_edges = graph.event_edges;
    persisted.obs_connections = backend.action_engine.obs_connections();
    persisted.shortcuts = backend.shortcut_engine.snapshot();
    persisted.node_layouts = graph.layouts;
    persisted.registered_nodes = backend.registry.snapshot();
    persisted.device_history = backend.device_history.read().clone();
    persisted.replay_cache_metadata = backend.replay_caches.snapshot();
    persisted.active_routing_profile = backend.active_routing_profile.lock().clone();
    persisted.completed_playback_device_operations =
        backend.completed_playback_device_operations.lock().clone();
    persist_configuration_candidate(backend, persisted)
}

pub fn insert_transform_on_edge(backend: &Backend, node_id: NodeId, edge_id: EdgeId) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let next = crate::routing::graph_edits::insert_transform(&graph, node_id, edge_id)?;
    backend
        .controller
        .commit_graph_candidate(graph.revision, next, |candidate| {
            save_graph_configuration(backend, candidate.clone())
        })?;
    backend.nodes.refresh_recorder_inputs();
    Ok(())
}

pub fn remove_nodes_with_bypass(backend: &Backend, node_ids: Vec<NodeId>) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let graph = backend.controller.snapshot();
    let next = crate::routing::graph_edits::remove_with_bypass(&graph, &node_ids)?;
    backend
        .controller
        .commit_graph_candidate(graph.revision, next, |candidate| {
            save_graph_configuration(backend, candidate.clone())
        })?;
    if backend
        .runtime
        .audition_target()
        .is_some_and(|target| node_ids.contains(&target.node_id))
    {
        stop_audition(backend);
    }
    for id in node_ids {
        if let Some(NodeProperties::TransformPlugin { instance_id, .. }) =
            graph.nodes.get(&id).map(|node| &node.properties)
        {
            if let Ok(plugin) = backend.plugins.get(*instance_id) {
                let _ = plugin.stop();
            }
            backend.plugins.remove(*instance_id);
        }
        if backend.nodes.is_active(id) {
            backend.nodes.stop_node(id)?;
        }
        backend.runtime.unbind_node(id);
    }
    backend.nodes.refresh_recorder_inputs();
    Ok(())
}

fn persist_configuration_candidate(
    backend: &Backend,
    persisted: PersistedConfiguration,
) -> Result<()> {
    // 作業状態だけでなく、切替時に読み戻す名前付きプロジェクトも更新する。
    // 同じSnapshotを使い、Graph・接続・配置の保存内容を一致させる。
    let mut active_profile_bytes = None;
    let mut active_profile_path = None;
    if let Some(profile_id) = persisted
        .active_routing_profile
        .as_deref()
        .filter(|id| routing_profile::is_user_project(id))
    {
        let mut profile = routing_profile::load(&backend.config, profile_id)?;
        crate::config::workflow_store::archive(
            &backend.config,
            profile_id,
            &serde_json::to_vec(&profile)?,
            false,
        )?;
        profile.version = ROUTING_PROFILE_SCHEMA_VERSION;
        profile.nodes = persisted.nodes.clone();
        profile.routing_edges = persisted.routing_edges.clone();
        profile.event_edges = persisted.event_edges.clone();
        profile.node_layouts = persisted.node_layouts.clone();
        profile.registered_nodes = persisted.registered_nodes.clone();
        profile.obs_connections = persisted.obs_connections.clone();
        profile.shortcuts = persisted.shortcuts.clone();
        profile.replay_cache_metadata = persisted.replay_cache_metadata.clone();
        active_profile_path = Some(routing_profile::profile_path(&backend.config, profile_id));
        active_profile_bytes = Some(serde_json::to_vec_pretty(&profile)?);
    }
    let config_bytes = serde_json::to_vec_pretty(&persisted)?;
    if let (Some(path), Some(profile_bytes)) = (active_profile_path, active_profile_bytes) {
        crate::config::transaction::commit_pair(
            &backend.config.base_dir(),
            (backend.config.path(), config_bytes),
            (&path, profile_bytes),
        )
    } else {
        backend.config.save(&persisted)
    }
}

pub fn load_configuration(backend: &Backend) -> Result<()> {
    let _operation = backend.operation_lock.lock();
    let mut persisted = backend.config.load()?;
    for node in persisted.registered_nodes.values_mut() {
        if node.node_type == NodeType::ReplayCache {
            migrate_legacy_replay_cache_name(&mut node.display_name);
        }
    }
    for node in persisted.nodes.values_mut() {
        if node.node_type == NodeType::ReplayCache {
            migrate_legacy_replay_cache_name(&mut node.display_name);
        }
    }
    let previous_obs_connections = backend.action_engine.obs_connections();
    let previous_shortcuts = backend.shortcut_engine.snapshot();
    backend
        .action_engine
        .replace_obs_connections(persisted.obs_connections.clone())?;
    backend
        .shortcut_engine
        .replace_bindings(persisted.shortcuts.clone())?;
    if let Err(error) = backend
        .controller
        .replace_graph(crate::audio::graph::GraphSnapshot {
            revision: 0,
            nodes: persisted.nodes,
            edges: persisted.routing_edges,
            event_edges: persisted.event_edges,
            layouts: persisted.node_layouts,
        })
    {
        let _ = backend
            .action_engine
            .replace_obs_connections(previous_obs_connections);
        let _ = backend.shortcut_engine.replace_bindings(previous_shortcuts);
        return Err(error);
    }
    backend.registry.replace_all(persisted.registered_nodes);
    *backend.device_history.write() = persisted.device_history;
    backend
        .replay_caches
        .replace(persisted.replay_cache_metadata);
    *backend.active_routing_profile.lock() = persisted.active_routing_profile;
    *backend.completed_playback_device_operations.lock() =
        persisted.completed_playback_device_operations;
    Ok(())
}

/// Convenience alias for Tauri managed state.
pub type SharedBackend = Arc<Backend>;

#[cfg(feature = "tauri")]
pub mod bindings {
    use std::{path::PathBuf, sync::Arc, time::Duration};
    use tauri::{Emitter, Manager, State};

    use crate::commands::tauri_commands as api;
    use crate::{
        actions::model::{
            ActionExecutionId, ActionNodeConfig, ActionRuntimeEvent, ActionRuntimeState,
            LogOutputConfig, ObsConnectionConfig, ObsConnectionId, SecretId,
        },
        audio::{
            engine::RuntimeEvent,
            graph::{
                ApplicationFilterMode, CacheId, CaptureMethod, ChannelMapperConfig,
                ConnectionEndpoint, DelayConfig, Edge, EdgeId, EventEdge, FilterRuleId,
                FolderReplayMode, GroupProcessorId, GroupProcessorSpec, InputSelectorConfig,
                MemoryReplayMode, NodeId, NodeLayout, NodeSpec, PluginInstanceId, PortId,
                ProcessFilterRule, ProcessFilterTarget, RecordingFormat, RegisteredNodeId,
                ReplayItemId, RvcInstanceId, SilenceDetectorConfig, SwitchPolicy, TestSignalConfig,
            },
        },
        config::model::ApplicationSettings,
        events::{BackendEvent, BackendEventMonitor},
        plugins::{
            instance::PluginState, manifest::PluginDescriptor, package::PluginDiscoveryReport,
        },
        profile::volume_profile::{ApplyVolumeProfileReport, VolumeMuteProfile, VolumeProfileId},
        recording::{
            state::{RecorderStateSnapshot, RecordingOutput},
            trim::TrimRange,
            waveform::WaveformBucket,
        },
        registry::{
            device_history::{AppDeviceId, DeviceUsage, KnownDevice},
            node_registry::RegisteredNode,
        },
        replay::{
            cache::{ReplayCacheConfig, ReplayCacheItem},
            state::ReplayStateSnapshot,
        },
        routing::{
            loop_detector::LoopWarning,
            traversal::RouteSubgraph,
            validator::{ConnectionCandidateResult, ValidationReport},
        },
        runtime_manager::NodeStartResult,
        rvc::adapter::RvcState,
        shortcut::model::{
            NodeActionDescriptor, ShortcutBinding, ShortcutId, ShortcutRuntimeEvent,
            ShortcutValidation,
        },
        windows::{
            devices::AudioDeviceInfo,
            implicit_routes::{ImplicitRoute, ListenDetection, VirtualDevicePair},
            input_monitor::{InputMonitorScope, InputMonitorSnapshot, InputMonitorStatesSnapshot},
            process_watcher::{FilteredCaptureStatus, ProcessInfo},
        },
        Backend,
    };

    type CmdResult<T> = std::result::Result<T, String>;
    fn backend<'a>(state: &'a State<'_, Arc<Backend>>) -> &'a Backend {
        state.inner().as_ref()
    }
    fn map<T>(result: crate::Result<T>) -> CmdResult<T> {
        result.map_err(|e| e.to_string())
    }
    async fn run_backend_blocking<T: Send + 'static>(
        backend: Arc<Backend>,
        operation: impl FnOnce(&Backend) -> crate::Result<T> + Send + 'static,
    ) -> CmdResult<T> {
        tauri::async_runtime::spawn_blocking(move || operation(&backend))
            .await
            .map_err(|error| format!("バックエンド操作Taskの実行に失敗しました: {error}"))?
            .map_err(|error| error.to_string())
    }

    #[tauri::command]
    pub fn frontend_snapshot(state: State<'_, Arc<Backend>>) -> CmdResult<api::FrontendSnapshot> {
        map(api::frontend_snapshot(backend(&state)))
    }

    #[tauri::command]
    pub fn input_endpoint_levels(state: State<'_, Arc<Backend>>) -> Vec<api::InputEndpointLevel> {
        api::input_endpoint_levels(backend(&state))
    }

    #[tauri::command]
    pub fn set_meter_monitor_targets(
        state: State<'_, Arc<Backend>>,
        registered_node_ids: Vec<RegisteredNodeId>,
    ) -> CmdResult<()> {
        map(api::set_meter_monitor_targets(
            backend(&state),
            registered_node_ids,
        ))
    }

    #[tauri::command]
    pub fn meter_monitor_levels(state: State<'_, Arc<Backend>>) -> Vec<api::InputEndpointLevel> {
        api::meter_monitor_levels(backend(&state))
    }

    #[tauri::command]
    pub fn open_windows_sound_settings(state: State<'_, Arc<Backend>>) -> CmdResult<()> {
        let _ = state;
        map(api::open_windows_sound_settings())
    }

    #[tauri::command]
    pub fn start_input_monitor(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        output_endpoint_id: Option<String>,
        scope: Option<InputMonitorScope>,
    ) -> CmdResult<InputMonitorSnapshot> {
        map(api::start_input_monitor(
            backend(&state),
            registered_node_id,
            output_endpoint_id,
            scope.unwrap_or_default(),
        ))
    }

    #[tauri::command]
    pub fn start_input_preview(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        output_endpoint_id: Option<String>,
        scope: Option<InputMonitorScope>,
    ) -> CmdResult<InputMonitorSnapshot> {
        map(api::start_input_preview(
            backend(&state),
            registered_node_id,
            output_endpoint_id,
            scope.unwrap_or_default(),
        ))
    }

    #[tauri::command]
    pub fn play_input_preview(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
    ) -> CmdResult<InputMonitorSnapshot> {
        map(api::play_input_preview(backend(&state), registered_node_id))
    }

    #[tauri::command]
    pub fn pause_input_preview(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
    ) -> CmdResult<InputMonitorSnapshot> {
        map(api::pause_input_preview(
            backend(&state),
            registered_node_id,
        ))
    }

    #[tauri::command]
    pub fn seek_input_preview(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        seconds: f64,
    ) -> CmdResult<InputMonitorSnapshot> {
        map(api::seek_input_preview(
            backend(&state),
            registered_node_id,
            seconds,
        ))
    }

    #[tauri::command]
    pub fn stop_input_monitor(state: State<'_, Arc<Backend>>) {
        api::stop_input_monitor(backend(&state));
    }
    #[tauri::command]
    pub fn workflow_action(
        state: State<'_, Arc<Backend>>,
        action: String,
        payload: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String> {
        map(api::workflow_action(backend(&state), action, payload))
    }
    #[tauri::command]
    pub fn start_audition(
        state: State<'_, Arc<Backend>>,
        target: crate::audio::audition::AuditionTarget,
        endpoint_id: String,
    ) -> CmdResult<()> {
        map(api::start_audition(backend(&state), target, endpoint_id))
    }
    #[tauri::command]
    pub fn stop_audition(state: State<'_, Arc<Backend>>) {
        api::stop_audition(backend(&state));
    }
    #[tauri::command]
    pub fn audition_state(
        state: State<'_, Arc<Backend>>,
    ) -> Option<crate::audio::audition::AuditionTarget> {
        api::audition_state(backend(&state))
    }
    #[tauri::command]
    pub fn audition_comparison_report(
        state: State<'_, Arc<Backend>>,
    ) -> crate::audio::comparison::ComparisonReport {
        api::audition_comparison_report(backend(&state))
    }
    #[tauri::command]
    pub async fn voice_model_action(
        state: State<'_, Arc<Backend>>,
        action: String,
        method: Option<String>,
        job_id: Option<String>,
    ) -> CmdResult<serde_json::Value> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            crate::commands::voice_models::execute(backend, action, method, job_id)
        })
        .await
    }

    #[tauri::command]
    pub fn stop_input_monitor_for(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
    ) -> InputMonitorStatesSnapshot {
        api::stop_input_monitor_for(backend(&state), registered_node_id)
    }

    #[tauri::command]
    pub fn stop_input_monitors_for_scope(
        state: State<'_, Arc<Backend>>,
        scope: InputMonitorScope,
    ) -> InputMonitorStatesSnapshot {
        api::stop_input_monitors_for_scope(backend(&state), scope)
    }

    #[tauri::command]
    pub fn input_monitor_state(state: State<'_, Arc<Backend>>) -> Option<InputMonitorSnapshot> {
        api::input_monitor_state(backend(&state))
    }

    #[tauri::command]
    pub fn input_monitor_states(state: State<'_, Arc<Backend>>) -> InputMonitorStatesSnapshot {
        api::input_monitor_states(backend(&state))
    }

    #[tauri::command]
    pub fn list_routing_profiles(
        state: State<'_, Arc<Backend>>,
    ) -> CmdResult<Vec<api::RoutingProfileSummary>> {
        map(api::list_routing_profiles(backend(&state)))
    }

    #[tauri::command]
    pub async fn load_routing_profile(
        state: State<'_, Arc<Backend>>,
        profile_id: String,
    ) -> CmdResult<api::RoutingProfileSummary> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::load_routing_profile(backend, profile_id)
        })
        .await
    }

    #[tauri::command]
    pub fn save_routing_project_as(
        state: State<'_, Arc<Backend>>,
        display_name: String,
    ) -> CmdResult<api::RoutingProfileSummary> {
        map(api::save_routing_project_as(backend(&state), display_name))
    }

    #[tauri::command]
    pub fn save_routing_project(
        state: State<'_, Arc<Backend>>,
    ) -> CmdResult<api::RoutingProfileSummary> {
        map(api::save_routing_project(backend(&state)))
    }

    #[tauri::command]
    pub async fn new_routing_project(state: State<'_, Arc<Backend>>) -> CmdResult<()> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::new_routing_project(backend)
        })
        .await
    }

    #[tauri::command]
    pub fn delete_routing_project(
        state: State<'_, Arc<Backend>>,
        profile_id: String,
    ) -> CmdResult<()> {
        map(api::delete_routing_project(backend(&state), profile_id))
    }

    #[tauri::command]
    pub fn list_input_devices(state: State<'_, Arc<Backend>>) -> CmdResult<Vec<AudioDeviceInfo>> {
        map(api::list_input_devices(backend(&state)))
    }
    #[tauri::command]
    pub fn list_output_devices(state: State<'_, Arc<Backend>>) -> CmdResult<Vec<AudioDeviceInfo>> {
        map(api::list_output_devices(backend(&state)))
    }
    #[tauri::command]
    pub async fn record_microphone_wav(
        request: api::MicrophoneRecordingRequest,
    ) -> CmdResult<api::MicrophoneRecordingReport> {
        tauri::async_runtime::spawn_blocking(move || api::record_microphone_wav(request))
            .await
            .map_err(|error| format!("マイク録音Taskの実行に失敗しました: {error}"))?
            .map_err(|error| error.to_string())
    }
    #[tauri::command]
    pub async fn refresh_devices(state: State<'_, Arc<Backend>>) -> CmdResult<api::DeviceLists> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::refresh_devices(backend)
        })
        .await
    }
    #[tauri::command]
    pub fn set_default_audio_device(
        state: State<'_, Arc<Backend>>,
        endpoint_id: String,
        direction: crate::windows::devices::DeviceDirection,
        communications: bool,
    ) -> CmdResult<api::DeviceLists> {
        map(api::set_default_audio_device_command(
            backend(&state),
            endpoint_id,
            direction,
            communications,
        ))
    }
    #[tauri::command]
    pub fn load_test_configuration_profile(
        state: State<'_, Arc<Backend>>,
    ) -> CmdResult<api::TestConfigurationProfileReport> {
        map(api::load_test_configuration_profile(backend(&state)))
    }
    #[tauri::command]
    pub fn list_known_devices(state: State<'_, Arc<Backend>>) -> Vec<KnownDevice> {
        api::list_known_devices(backend(&state))
    }
    #[tauri::command]
    pub fn set_known_device_usage(
        state: State<'_, Arc<Backend>>,
        app_device_id: AppDeviceId,
        usage: DeviceUsage,
        enabled: bool,
    ) -> CmdResult<()> {
        map(api::set_known_device_usage(
            backend(&state),
            app_device_id,
            usage,
            enabled,
        ))
    }
    #[tauri::command]
    pub fn forget_device(
        state: State<'_, Arc<Backend>>,
        app_device_id: AppDeviceId,
    ) -> CmdResult<()> {
        map(api::forget_device(backend(&state), app_device_id))
    }
    #[tauri::command]
    pub fn update_device_visibility(
        state: State<'_, Arc<Backend>>,
        endpoint_id: String,
        visible: bool,
    ) -> CmdResult<()> {
        map(api::update_device_visibility(
            backend(&state),
            &endpoint_id,
            visible,
        ))
    }
    #[tauri::command]
    pub fn update_device_order(
        state: State<'_, Arc<Backend>>,
        endpoint_id: String,
        order: i32,
    ) -> CmdResult<()> {
        map(api::update_device_order(
            backend(&state),
            &endpoint_id,
            order,
        ))
    }

    #[tauri::command]
    pub fn list_processes() -> Vec<ProcessInfo> {
        api::list_processes_command()
    }
    #[tauri::command]
    pub async fn list_device_apps(
        endpoint_id: String,
    ) -> CmdResult<Vec<crate::windows::device_apps::DeviceApp>> {
        tauri::async_runtime::spawn_blocking(move || {
            crate::windows::device_apps::list_device_apps(endpoint_id)
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[tauri::command]
    pub fn list_audio_processes() -> Vec<ProcessInfo> {
        api::list_audio_processes_command()
    }
    #[tauri::command]
    pub fn list_process_filter_targets() -> Vec<api::ProcessFilterTargetInfo> {
        api::list_process_filter_targets()
    }
    #[tauri::command]
    pub fn get_settings(state: State<'_, Arc<Backend>>) -> CmdResult<ApplicationSettings> {
        map(api::get_settings(backend(&state)))
    }
    #[tauri::command]
    pub async fn update_settings(
        state: State<'_, Arc<Backend>>,
        settings: ApplicationSettings,
    ) -> CmdResult<ApplicationSettings> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::update_settings(backend, settings)
        })
        .await
    }
    #[tauri::command]
    pub fn apply_latency_preset(
        state: State<'_, Arc<Backend>>,
        preset: crate::config::model::LatencyPreset,
    ) -> CmdResult<api::ApplyLatencyPresetResult> {
        map(api::apply_latency_preset(backend(&state), preset))
    }

    #[tauri::command]
    pub fn register_fixed_file_input(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        path: PathBuf,
        loop_enabled: bool,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_fixed_file_input(
            backend(&state),
            display_name,
            path,
            loop_enabled,
        ))
    }
    #[tauri::command]
    pub fn register_edge_tab_input(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        stream_key: String,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_edge_tab_input(
            backend(&state),
            display_name,
            stream_key,
        ))
    }
    #[tauri::command]
    pub fn get_edge_auth_token(state: State<'_, Arc<Backend>>) -> String {
        api::get_edge_auth_token(backend(&state))
    }
    #[tauri::command]
    pub fn regenerate_edge_token(state: State<'_, Arc<Backend>>) -> String {
        api::regenerate_edge_token(backend(&state))
    }
    #[tauri::command]
    pub fn list_edge_tabs(
        state: State<'_, Arc<Backend>>,
    ) -> Vec<crate::edge::receiver::EdgeTabSummary> {
        api::list_edge_tabs(backend(&state))
    }
    #[tauri::command]
    pub async fn control_edge_media(
        state: State<'_, Arc<Backend>>,
        stream_key: String,
        action: crate::edge::control::EdgeMediaAction,
    ) -> CmdResult<crate::edge::receiver::EdgeMediaResult> {
        map(api::control_edge_media(backend(&state), stream_key, action).await)
    }
    #[tauri::command]
    pub fn register_process_pid_input(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        pid: u32,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_process_pid_input(
            backend(&state),
            display_name,
            pid,
        ))
    }
    #[tauri::command]
    pub fn register_process_exe_input(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        exe_path: PathBuf,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_process_exe_input(
            backend(&state),
            display_name,
            exe_path,
        ))
    }
    #[tauri::command]
    pub fn register_transform_plugin(
        state: State<'_, Arc<Backend>>,
        request: api::TransformPluginCreateRequest,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_transform_plugin(backend(&state), request))
    }
    #[tauri::command]
    pub fn register_multi_input(
        state: State<'_, Arc<Backend>>,
        display_name: String,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_multi_input(backend(&state), display_name))
    }
    #[tauri::command]
    pub fn register_test_signal(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        config: TestSignalConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_test_signal(
            backend(&state),
            display_name,
            config,
        ))
    }
    #[tauri::command]
    pub fn register_input_selector(
        state: State<'_, Arc<Backend>>,
        display_name: String,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_input_selector(backend(&state), display_name))
    }
    #[tauri::command]
    pub fn register_delay(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        config: DelayConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_delay(backend(&state), display_name, config))
    }
    #[tauri::command]
    pub fn register_channel_mapper(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        config: ChannelMapperConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_channel_mapper(
            backend(&state),
            display_name,
            config,
        ))
    }
    #[tauri::command]
    pub fn register_silence_detector(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        config: SilenceDetectorConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_silence_detector(
            backend(&state),
            display_name,
            config,
        ))
    }
    #[tauri::command]
    pub fn register_folder_input(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        path: PathBuf,
        mode: FolderReplayMode,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_folder_input(
            backend(&state),
            display_name,
            path,
            mode,
        ))
    }
    #[tauri::command]
    pub fn create_replay_cache(
        state: State<'_, Arc<Backend>>,
        config: ReplayCacheConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::create_replay_cache(backend(&state), config))
    }
    #[tauri::command]
    pub fn list_replay_caches(state: State<'_, Arc<Backend>>) -> Vec<ReplayCacheConfig> {
        api::list_replay_caches(backend(&state))
    }
    #[tauri::command]
    pub fn register_memory_replay(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        cache_id: CacheId,
        mode: MemoryReplayMode,
        selected_item_id: Option<ReplayItemId>,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_memory_replay(
            backend(&state),
            display_name,
            cache_id,
            mode,
            selected_item_id,
        ))
    }
    #[tauri::command]
    pub fn register_folder_recorder(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        directory: PathBuf,
        format: RecordingFormat,
        auto_record: bool,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_folder_recorder(
            backend(&state),
            display_name,
            directory,
            format,
            auto_record,
        ))
    }
    #[tauri::command]
    pub fn import_replay_cache_item(
        state: State<'_, Arc<Backend>>,
        cache_id: CacheId,
        path: PathBuf,
        created_unix_ms: u64,
    ) -> CmdResult<ReplayCacheItem> {
        map(api::import_replay_cache_item(
            backend(&state),
            cache_id,
            path,
            created_unix_ms,
        ))
    }
    #[tauri::command]
    pub fn list_memory_items(
        state: State<'_, Arc<Backend>>,
        cache_id: CacheId,
    ) -> CmdResult<Vec<ReplayCacheItem>> {
        map(api::list_memory_items(backend(&state), cache_id))
    }

    #[tauri::command]
    pub fn list_transform_plugins(state: State<'_, Arc<Backend>>) -> Vec<PluginDescriptor> {
        api::list_transform_plugins(backend(&state))
    }
    #[tauri::command]
    pub fn transform_plugin_discovery_report(
        state: State<'_, Arc<Backend>>,
    ) -> PluginDiscoveryReport {
        api::transform_plugin_discovery_report(backend(&state))
    }
    #[tauri::command]
    pub async fn refresh_transform_plugins(
        state: State<'_, Arc<Backend>>,
    ) -> CmdResult<PluginDiscoveryReport> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::refresh_transform_plugins(backend)
        })
        .await
    }
    #[tauri::command]
    pub fn create_transform_plugin(
        state: State<'_, Arc<Backend>>,
        request: api::TransformPluginCreateRequest,
    ) -> CmdResult<NodeSpec> {
        map(api::create_transform_plugin(backend(&state), request))
    }
    #[tauri::command]
    pub async fn set_transform_plugin_parameter(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        parameter_id: String,
        value: serde_json::Value,
    ) -> CmdResult<NodeSpec> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::set_transform_plugin_parameter(backend, node_id, parameter_id, value)
        })
        .await
    }
    #[tauri::command]
    pub fn set_transform_plugin_bypass(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        bypass: bool,
    ) -> CmdResult<NodeSpec> {
        map(api::set_transform_plugin_bypass(
            backend(&state),
            node_id,
            bypass,
        ))
    }
    #[tauri::command]
    pub fn save_node_preset(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        name: String,
        overwrite: bool,
    ) -> CmdResult<RegisteredNode> {
        map(api::save_node_preset(
            backend(&state),
            node_id,
            name,
            overwrite,
        ))
    }
    #[tauri::command]
    pub fn apply_node_preset(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        preset_name: String,
    ) -> CmdResult<NodeSpec> {
        map(api::apply_node_preset(
            backend(&state),
            node_id,
            preset_name,
        ))
    }
    #[tauri::command]
    pub fn set_transform_plugin_switch_policy(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        switch_policy: SwitchPolicy,
    ) -> CmdResult<NodeSpec> {
        map(api::set_transform_plugin_switch_policy(
            backend(&state),
            node_id,
            switch_policy,
        ))
    }
    #[tauri::command]
    pub fn get_plugin_stats(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<api::PluginStatsReport> {
        map(api::get_plugin_stats(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn transform_plugin_state(
        state: State<'_, Arc<Backend>>,
        instance_id: PluginInstanceId,
    ) -> CmdResult<PluginState> {
        map(api::transform_plugin_state(backend(&state), instance_id))
    }

    #[tauri::command]
    pub fn list_registered_nodes(state: State<'_, Arc<Backend>>) -> Vec<RegisteredNode> {
        api::list_registered_nodes(backend(&state))
    }
    #[tauri::command]
    pub fn update_registered_node(
        state: State<'_, Arc<Backend>>,
        node: RegisteredNode,
    ) -> CmdResult<RegisteredNode> {
        map(api::update_registered_node(backend(&state), node))
    }
    #[tauri::command]
    pub fn set_input_capture_method(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        capture_method: CaptureMethod,
    ) -> CmdResult<RegisteredNode> {
        map(api::set_input_capture_method(
            backend(&state),
            registered_node_id,
            capture_method,
        ))
    }
    #[tauri::command]
    pub fn reconnect_input_node(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<NodeStartResult> {
        map(api::reconnect_input_node(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn set_registered_node_enabled(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        enabled: bool,
    ) -> CmdResult<RegisteredNode> {
        map(api::set_registered_node_enabled(
            backend(&state),
            registered_node_id,
            enabled,
        ))
    }
    #[tauri::command]
    pub fn update_registered_node_comment(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        comment: String,
    ) -> CmdResult<RegisteredNode> {
        map(api::update_registered_node_comment(
            backend(&state),
            registered_node_id,
            comment,
        ))
    }
    #[tauri::command]
    pub fn set_show_in_playback(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        show: bool,
    ) -> CmdResult<RegisteredNode> {
        map(api::set_show_in_playback(
            backend(&state),
            registered_node_id,
            show,
        ))
    }
    #[tauri::command]
    pub fn set_playback_order(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
        order: i32,
    ) -> CmdResult<RegisteredNode> {
        map(api::set_playback_order(
            backend(&state),
            registered_node_id,
            order,
        ))
    }
    #[tauri::command]
    pub fn delete_registered_node(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
    ) -> CmdResult<()> {
        map(api::delete_registered_node(
            backend(&state),
            registered_node_id,
        ))
    }
    #[tauri::command]
    pub fn duplicate_registered_node(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
    ) -> CmdResult<RegisteredNode> {
        map(api::duplicate_registered_node(
            backend(&state),
            registered_node_id,
        ))
    }
    #[tauri::command]
    pub async fn add_graph_node(
        state: State<'_, Arc<Backend>>,
        registered_node_id: RegisteredNodeId,
    ) -> CmdResult<NodeSpec> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::add_graph_node(backend, registered_node_id)
        })
        .await
    }
    #[tauri::command]
    pub fn place_playback_device(
        state: State<'_, Arc<Backend>>,
        request: api::PlaybackDevicePlacementRequest,
    ) -> CmdResult<api::PlaybackDevicePlacementResult> {
        map(api::place_playback_device(backend(&state), request))
    }
    #[tauri::command]
    pub fn create_multi_input_instance(
        state: State<'_, Arc<Backend>>,
        display_name: String,
    ) -> CmdResult<NodeSpec> {
        map(api::create_multi_input_instance(
            backend(&state),
            display_name,
        ))
    }
    #[tauri::command]
    pub fn create_memory_replay_instance(
        state: State<'_, Arc<Backend>>,
        cache_id: CacheId,
        mode: MemoryReplayMode,
    ) -> CmdResult<NodeSpec> {
        map(api::create_memory_replay_instance(
            backend(&state),
            cache_id,
            mode,
        ))
    }
    #[tauri::command]
    pub fn remove_graph_node(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::remove_graph_node(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn duplicate_graph_node(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<NodeSpec> {
        map(api::duplicate_graph_node(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn set_graph_runtime_enabled(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        enabled: bool,
    ) -> CmdResult<NodeSpec> {
        map(api::set_graph_runtime_enabled(
            backend(&state),
            node_id,
            enabled,
        ))
    }
    #[tauri::command]
    pub fn update_node_comment(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        comment: String,
    ) -> CmdResult<NodeSpec> {
        map(api::update_node_comment(backend(&state), node_id, comment))
    }
    #[tauri::command]
    pub fn set_graph_playback_display(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        show_in_playback: bool,
        playback_order: i32,
    ) -> CmdResult<NodeSpec> {
        map(api::set_graph_playback_display(
            backend(&state),
            node_id,
            show_in_playback,
            playback_order,
        ))
    }
    #[tauri::command]
    pub fn update_node_layout(
        state: State<'_, Arc<Backend>>,
        layout: NodeLayout,
    ) -> CmdResult<NodeLayout> {
        map(api::update_node_layout(backend(&state), layout))
    }

    #[tauri::command]
    pub fn create_node(state: State<'_, Arc<Backend>>, node: NodeSpec) -> CmdResult<NodeSpec> {
        map(api::create_node(backend(&state), node))
    }
    #[tauri::command]
    pub fn update_node(state: State<'_, Arc<Backend>>, node: NodeSpec) -> CmdResult<NodeSpec> {
        map(api::update_node(backend(&state), node))
    }
    #[tauri::command]
    pub fn delete_node(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::delete_node(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn list_nodes(state: State<'_, Arc<Backend>>) -> Vec<NodeSpec> {
        api::list_nodes(backend(&state))
    }
    #[tauri::command]
    pub fn create_audio_processing_group(
        state: State<'_, Arc<Backend>>,
        node_ids: Vec<NodeId>,
        position: api::GraphPosition,
        ordered_node_ids: Option<Vec<NodeId>>,
    ) -> CmdResult<NodeSpec> {
        map(api::create_audio_processing_group(
            backend(&state),
            node_ids,
            position,
            ordered_node_ids,
        ))
    }
    #[tauri::command]
    pub fn set_audio_processing_group(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        flow: Vec<GroupProcessorSpec>,
    ) -> CmdResult<NodeSpec> {
        map(api::set_audio_processing_group(
            backend(&state),
            node_id,
            flow,
        ))
    }
    #[tauri::command]
    pub fn ungroup_audio_processing(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        grid_spacing: f32,
    ) -> CmdResult<Vec<NodeSpec>> {
        map(api::ungroup_audio_processing(
            backend(&state),
            node_id,
            grid_spacing,
        ))
    }
    #[tauri::command]
    pub fn set_audio_processing_group_processor_parameter(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        processor_id: GroupProcessorId,
        parameter_id: String,
        value: serde_json::Value,
    ) -> CmdResult<NodeSpec> {
        map(api::set_audio_processing_group_processor_parameter(
            backend(&state),
            node_id,
            processor_id,
            parameter_id,
            value,
        ))
    }
    #[tauri::command]
    pub fn export_graph_nodes(
        state: State<'_, Arc<Backend>>,
        node_ids: Vec<NodeId>,
    ) -> CmdResult<serde_json::Value> {
        map(api::export_graph_nodes(backend(&state), node_ids))
    }
    #[tauri::command]
    pub fn paste_graph_nodes(
        state: State<'_, Arc<Backend>>,
        payload: serde_json::Value,
        position: api::GraphPosition,
    ) -> CmdResult<Vec<NodeSpec>> {
        map(api::paste_graph_nodes(backend(&state), payload, position))
    }
    #[tauri::command]
    pub fn delete_graph_nodes(
        state: State<'_, Arc<Backend>>,
        node_ids: Vec<NodeId>,
    ) -> CmdResult<()> {
        map(api::delete_graph_nodes(backend(&state), node_ids))
    }
    #[tauri::command]
    pub fn set_application_filter_mode(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        mode: ApplicationFilterMode,
    ) -> CmdResult<NodeSpec> {
        map(api::set_application_filter_mode(
            backend(&state),
            node_id,
            mode,
        ))
    }
    #[tauri::command]
    pub fn add_process_filter_rule(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        target: ProcessFilterTarget,
    ) -> CmdResult<ProcessFilterRule> {
        map(api::add_process_filter_rule(
            backend(&state),
            node_id,
            target,
        ))
    }
    #[tauri::command]
    pub fn update_process_filter_rule(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        rule: ProcessFilterRule,
    ) -> CmdResult<ProcessFilterRule> {
        map(api::update_process_filter_rule(
            backend(&state),
            node_id,
            rule,
        ))
    }
    #[tauri::command]
    pub fn remove_process_filter_rule(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        rule_id: FilterRuleId,
    ) -> CmdResult<()> {
        map(api::remove_process_filter_rule(
            backend(&state),
            node_id,
            rule_id,
        ))
    }
    #[tauri::command]
    pub fn get_filtered_capture_status(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> Option<FilteredCaptureStatus> {
        api::get_filtered_capture_status(backend(&state), node_id)
    }
    #[tauri::command]
    pub fn add_multi_input_port(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        name: String,
    ) -> CmdResult<PortId> {
        map(api::add_multi_input_port(backend(&state), node_id, name))
    }
    #[tauri::command]
    pub fn remove_port(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        port_id: PortId,
    ) -> CmdResult<()> {
        map(api::remove_port(backend(&state), node_id, port_id))
    }

    #[tauri::command]
    pub fn connect_nodes(
        state: State<'_, Arc<Backend>>,
        from_node: NodeId,
        from_port: PortId,
        to_node: NodeId,
        to_port: PortId,
    ) -> CmdResult<Edge> {
        map(api::connect_nodes(
            backend(&state),
            from_node,
            from_port,
            to_node,
            to_port,
        ))
    }
    #[tauri::command]
    pub fn connect_nodes_auto(
        state: State<'_, Arc<Backend>>,
        from_node: NodeId,
        to_node: NodeId,
    ) -> CmdResult<Edge> {
        map(api::connect_nodes_auto(backend(&state), from_node, to_node))
    }
    #[tauri::command]
    pub fn connect_endpoints(
        state: State<'_, Arc<Backend>>,
        first: ConnectionEndpoint,
        second: ConnectionEndpoint,
    ) -> CmdResult<Edge> {
        map(api::connect_endpoints(backend(&state), first, second))
    }
    #[tauri::command]
    pub fn list_event_edges(state: State<'_, Arc<Backend>>) -> Vec<EventEdge> {
        api::list_event_edges(backend(&state))
    }
    #[tauri::command]
    pub fn connect_event_nodes(
        state: State<'_, Arc<Backend>>,
        from_node: NodeId,
        from_port: PortId,
        to_node: NodeId,
        to_port: PortId,
    ) -> CmdResult<EventEdge> {
        map(api::connect_event_nodes(
            backend(&state),
            from_node,
            from_port,
            to_node,
            to_port,
        ))
    }
    #[tauri::command]
    pub fn connect_event_endpoints(
        state: State<'_, Arc<Backend>>,
        first: ConnectionEndpoint,
        second: ConnectionEndpoint,
    ) -> CmdResult<EventEdge> {
        map(api::connect_event_endpoints(backend(&state), first, second))
    }
    #[tauri::command]
    pub fn validate_connection_candidate(
        state: State<'_, Arc<Backend>>,
        source: NodeId,
        destination: NodeId,
    ) -> ConnectionCandidateResult {
        api::validate_connection_candidate_command(backend(&state), source, destination)
    }
    #[tauri::command]
    pub fn disconnect_nodes(state: State<'_, Arc<Backend>>, edge_id: EdgeId) -> CmdResult<()> {
        map(api::disconnect_nodes(backend(&state), edge_id))
    }
    #[tauri::command]
    pub fn disconnect_edge(state: State<'_, Arc<Backend>>, edge_id: EdgeId) -> CmdResult<()> {
        map(api::disconnect_edge(backend(&state), edge_id))
    }
    #[tauri::command]
    pub fn set_edge_controls(
        state: State<'_, Arc<Backend>>,
        edge_id: EdgeId,
        gain_db: f32,
        mute: bool,
    ) -> CmdResult<Edge> {
        map(api::set_edge_controls(
            backend(&state),
            edge_id,
            gain_db,
            mute,
        ))
    }
    #[tauri::command]
    pub fn list_edges(state: State<'_, Arc<Backend>>) -> Vec<Edge> {
        api::list_edges(backend(&state))
    }
    #[tauri::command]
    pub fn set_edge_color(
        state: State<'_, Arc<Backend>>,
        edge_id: EdgeId,
        color: Option<String>,
    ) -> CmdResult<Edge> {
        map(api::set_edge_color(backend(&state), edge_id, color))
    }
    #[tauri::command]
    pub async fn restore_transform_settings(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        parameters: std::collections::BTreeMap<String, serde_json::Value>,
        bypass: bool,
    ) -> CmdResult<NodeSpec> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::restore_transform_settings(backend, node_id, parameters, bypass)
        })
        .await
    }
    #[tauri::command]
    pub fn insert_transform_on_edge(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        edge_id: EdgeId,
    ) -> CmdResult<()> {
        map(api::insert_transform_on_edge(
            backend(&state),
            node_id,
            edge_id,
        ))
    }
    #[tauri::command]
    pub fn remove_nodes_with_bypass(
        state: State<'_, Arc<Backend>>,
        node_ids: Vec<NodeId>,
    ) -> CmdResult<()> {
        map(api::remove_nodes_with_bypass(backend(&state), node_ids))
    }
    #[tauri::command]
    pub fn validate_graph(state: State<'_, Arc<Backend>>) -> ValidationReport {
        api::validate_graph(backend(&state))
    }
    #[tauri::command]
    pub fn get_upstream_nodes(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<Vec<NodeId>> {
        map(api::get_upstream_nodes(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn get_downstream_nodes(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<Vec<NodeId>> {
        map(api::get_downstream_nodes(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn get_related_route(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<Vec<NodeId>> {
        map(api::get_related_route(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn get_route_to_output(
        state: State<'_, Arc<Backend>>,
        output_node: NodeId,
    ) -> CmdResult<RouteSubgraph> {
        map(api::get_route_to_output(backend(&state), output_node))
    }

    #[tauri::command]
    pub async fn start_node(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<NodeStartResult> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::start_node(backend, node_id)
        })
        .await
    }
    #[tauri::command]
    pub async fn stop_node(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::stop_node(backend, node_id)
        })
        .await
    }
    #[tauri::command]
    pub async fn start_graph(state: State<'_, Arc<Backend>>) -> CmdResult<Vec<NodeStartResult>> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::start_graph(backend)
        })
        .await
    }
    #[tauri::command]
    pub async fn stop_graph(state: State<'_, Arc<Backend>>) -> CmdResult<()> {
        run_backend_blocking(state.inner().clone(), move |backend| {
            api::stop_graph(backend);
            Ok(())
        })
        .await
    }
    #[tauri::command]
    pub fn recover_after_resume(state: State<'_, Arc<Backend>>) -> CmdResult<Vec<NodeStartResult>> {
        map(api::recover_after_resume(backend(&state)))
    }
    #[tauri::command]
    pub fn set_gain(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        gain_db: f32,
    ) -> CmdResult<NodeSpec> {
        map(api::set_gain(backend(&state), node_id, gain_db))
    }
    #[tauri::command]
    pub fn set_mute(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        mute: bool,
    ) -> CmdResult<NodeSpec> {
        map(api::set_mute(backend(&state), node_id, mute))
    }
    #[tauri::command]
    pub fn set_multi_input_slot(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        port_id: PortId,
        gain_db: f32,
        mute: bool,
    ) -> CmdResult<NodeSpec> {
        map(api::set_multi_input_slot(
            backend(&state),
            node_id,
            port_id,
            gain_db,
            mute,
        ))
    }

    #[tauri::command]
    pub fn get_test_signal_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<TestSignalConfig> {
        map(api::get_test_signal_config(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn update_test_signal(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: TestSignalConfig,
    ) -> CmdResult<TestSignalConfig> {
        map(api::update_test_signal(backend(&state), node_id, config))
    }
    #[tauri::command]
    pub fn get_input_selector_state(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<InputSelectorConfig> {
        map(api::get_input_selector_state(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn set_input_selector_active_port(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        active_port_id: Option<PortId>,
    ) -> CmdResult<InputSelectorConfig> {
        map(api::set_input_selector_active_port(
            backend(&state),
            node_id,
            active_port_id,
        ))
    }
    #[tauri::command]
    pub fn set_input_selector_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: InputSelectorConfig,
    ) -> CmdResult<InputSelectorConfig> {
        map(api::set_input_selector_config(
            backend(&state),
            node_id,
            config,
        ))
    }
    #[tauri::command]
    pub fn get_switcher_runtime(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> crate::nodes::transform::input_selector::SwitcherSnapshot {
        api::get_switcher_runtime(backend(&state), node_id)
    }
    #[tauri::command]
    pub fn select_input_by_index(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        index: usize,
    ) -> CmdResult<InputSelectorConfig> {
        map(api::select_input_by_index(backend(&state), node_id, index))
    }
    #[tauri::command]
    pub fn select_next_input(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<InputSelectorConfig> {
        map(api::select_next_input(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn select_previous_input(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<InputSelectorConfig> {
        map(api::select_previous_input(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn get_delay_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<DelayConfig> {
        map(api::get_delay_config(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn update_delay(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: DelayConfig,
    ) -> CmdResult<DelayConfig> {
        map(api::update_delay(backend(&state), node_id, config))
    }
    #[tauri::command]
    pub fn get_channel_mapper_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<ChannelMapperConfig> {
        map(api::get_channel_mapper_config(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn update_channel_mapper(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: ChannelMapperConfig,
    ) -> CmdResult<ChannelMapperConfig> {
        map(api::update_channel_mapper(backend(&state), node_id, config))
    }
    #[tauri::command]
    pub fn get_silence_detector_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<SilenceDetectorConfig> {
        map(api::get_silence_detector_config(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn update_silence_detector(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: SilenceDetectorConfig,
    ) -> CmdResult<SilenceDetectorConfig> {
        map(api::update_silence_detector(
            backend(&state),
            node_id,
            config,
        ))
    }
    #[tauri::command]
    pub fn get_silence_detector_state(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<crate::nodes::transform::silence_detector::SilenceDetectorStateSnapshot> {
        map(api::get_silence_detector_state(backend(&state), node_id))
    }

    #[tauri::command]
    pub fn list_volume_profiles(
        state: State<'_, Arc<Backend>>,
    ) -> CmdResult<Vec<VolumeMuteProfile>> {
        map(api::list_volume_profiles(backend(&state)))
    }
    #[tauri::command]
    pub fn save_volume_profile(
        state: State<'_, Arc<Backend>>,
        profile_id: Option<VolumeProfileId>,
        display_name: String,
    ) -> CmdResult<VolumeMuteProfile> {
        map(api::save_volume_profile(
            backend(&state),
            profile_id,
            display_name,
        ))
    }
    #[tauri::command]
    pub fn apply_volume_profile(
        state: State<'_, Arc<Backend>>,
        profile_id: VolumeProfileId,
    ) -> CmdResult<ApplyVolumeProfileReport> {
        map(api::apply_volume_profile(backend(&state), profile_id))
    }
    #[tauri::command]
    pub fn delete_volume_profile(
        state: State<'_, Arc<Backend>>,
        profile_id: VolumeProfileId,
    ) -> CmdResult<()> {
        map(api::delete_volume_profile(backend(&state), profile_id))
    }

    #[tauri::command]
    pub fn pause_file(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::pause_file(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn resume_file(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::resume_file(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn seek_file(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        seconds: f64,
    ) -> CmdResult<()> {
        map(api::seek_file(backend(&state), node_id, seconds))
    }
    #[tauri::command]
    pub fn file_position(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<f64> {
        map(api::file_position(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn trigger_replay(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::trigger_replay(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn play_replay(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::play_replay(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn pause_replay(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::pause_replay(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn stop_replay(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::stop_replay(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn seek_replay(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        seconds: f64,
    ) -> CmdResult<()> {
        map(api::seek_replay(backend(&state), node_id, seconds))
    }
    #[tauri::command]
    pub fn get_replay_state(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<ReplayStateSnapshot> {
        map(api::get_replay_state(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn list_folder_files(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<Vec<PathBuf>> {
        map(api::list_folder_files(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn select_folder_file(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        path: PathBuf,
    ) -> CmdResult<NodeSpec> {
        map(api::select_folder_file(backend(&state), node_id, path))
    }
    #[tauri::command]
    pub fn set_folder_replay_mode(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        mode: FolderReplayMode,
    ) -> CmdResult<NodeSpec> {
        map(api::set_folder_replay_mode(backend(&state), node_id, mode))
    }
    #[tauri::command]
    pub fn select_memory_item(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        item_id: ReplayItemId,
    ) -> CmdResult<NodeSpec> {
        map(api::select_memory_item(backend(&state), node_id, item_id))
    }
    #[tauri::command]
    pub fn set_memory_cache(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        cache_id: CacheId,
    ) -> CmdResult<NodeSpec> {
        map(api::set_memory_cache(backend(&state), node_id, cache_id))
    }

    #[tauri::command]
    pub fn start_recording(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<NodeStartResult> {
        map(api::start_recording(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn stop_recording(state: State<'_, Arc<Backend>>, node_id: NodeId) -> CmdResult<()> {
        map(api::stop_recording(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn start_recorder(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<RecorderStateSnapshot> {
        map(api::start_recorder(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn stop_recorder(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<Option<RecordingOutput>> {
        map(api::stop_recorder(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn set_auto_record(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        auto_record: bool,
    ) -> CmdResult<RecorderStateSnapshot> {
        map(api::set_auto_record(backend(&state), node_id, auto_record))
    }
    #[tauri::command]
    pub fn get_recorder_state(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<RecorderStateSnapshot> {
        map(api::get_recorder_state(backend(&state), node_id))
    }
    #[tauri::command]
    pub fn list_recordings(state: State<'_, Arc<Backend>>) -> Vec<api::RecordingInfo> {
        api::list_recordings(backend(&state))
    }
    #[tauri::command]
    pub fn get_waveform(path: PathBuf, buckets: usize) -> CmdResult<Vec<WaveformBucket>> {
        map(api::get_waveform_command(path, buckets))
    }
    #[tauri::command]
    pub fn get_cached_waveform(
        state: State<'_, Arc<Backend>>,
        path: PathBuf,
        buckets: usize,
    ) -> CmdResult<Vec<WaveformBucket>> {
        map(api::get_cached_waveform_command(
            backend(&state),
            path,
            buckets,
        ))
    }
    #[tauri::command]
    pub fn trim_recording(input: PathBuf, output: PathBuf, range: TrimRange) -> CmdResult<()> {
        map(api::trim_recording(input, output, range))
    }

    #[tauri::command]
    pub fn list_rvc_instances(state: State<'_, Arc<Backend>>) -> Vec<RvcInstanceId> {
        api::list_rvc_instances(backend(&state))
    }
    #[tauri::command]
    pub fn configure_rvc(
        state: State<'_, Arc<Backend>>,
        config: api::RvcConfiguration,
    ) -> CmdResult<()> {
        map(api::configure_rvc(backend(&state), config))
    }
    #[tauri::command]
    pub fn enable_rvc(
        state: State<'_, Arc<Backend>>,
        instance_id: RvcInstanceId,
        enabled: bool,
    ) -> CmdResult<()> {
        map(api::enable_rvc(backend(&state), instance_id, enabled))
    }
    #[tauri::command]
    pub fn bypass_rvc(
        state: State<'_, Arc<Backend>>,
        instance_id: RvcInstanceId,
        bypass: bool,
    ) -> CmdResult<()> {
        map(api::bypass_rvc(backend(&state), instance_id, bypass))
    }
    #[tauri::command]
    pub fn rvc_state(
        state: State<'_, Arc<Backend>>,
        instance_id: RvcInstanceId,
    ) -> CmdResult<RvcState> {
        map(api::rvc_state(backend(&state), instance_id))
    }

    #[tauri::command]
    pub fn list_virtual_device_pairs(
        state: State<'_, Arc<Backend>>,
    ) -> CmdResult<Vec<VirtualDevicePair>> {
        map(api::list_virtual_device_pairs(backend(&state)))
    }
    #[tauri::command]
    pub fn upsert_virtual_device_pair(
        state: State<'_, Arc<Backend>>,
        pair: VirtualDevicePair,
    ) -> CmdResult<VirtualDevicePair> {
        map(api::upsert_virtual_device_pair(backend(&state), pair))
    }
    #[tauri::command]
    pub fn delete_virtual_device_pair(state: State<'_, Arc<Backend>>, id: String) -> CmdResult<()> {
        map(api::delete_virtual_device_pair(backend(&state), &id))
    }
    #[tauri::command]
    pub fn detect_listen_route(capture_endpoint_id: String) -> ListenDetection {
        api::detect_listen_route(&capture_endpoint_id)
    }
    #[tauri::command]
    pub fn list_implicit_routes(state: State<'_, Arc<Backend>>) -> CmdResult<Vec<ImplicitRoute>> {
        map(api::list_implicit_routes_command(backend(&state)))
    }
    #[tauri::command]
    pub fn get_loop_warnings(state: State<'_, Arc<Backend>>) -> Vec<LoopWarning> {
        api::get_loop_warnings(backend(&state))
    }
    #[tauri::command]
    pub fn get_latency_report(
        state: State<'_, Arc<Backend>>,
    ) -> Vec<crate::audio::clock::PathLatencyReport> {
        api::get_latency_report(backend(&state))
    }
    #[tauri::command]
    pub fn save_current_configuration(state: State<'_, Arc<Backend>>) -> CmdResult<()> {
        map(api::save_current_configuration(backend(&state)))
    }
    #[tauri::command]
    pub fn load_configuration(state: State<'_, Arc<Backend>>) -> CmdResult<()> {
        map(api::load_configuration(backend(&state)))
    }

    #[tauri::command]
    pub fn register_action_node(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        config: ActionNodeConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_action_node(
            backend(&state),
            display_name,
            config,
        ))
    }

    #[tauri::command]
    pub fn register_log_output(
        state: State<'_, Arc<Backend>>,
        display_name: String,
        config: LogOutputConfig,
    ) -> CmdResult<RegisteredNode> {
        map(api::register_log_output(
            backend(&state),
            display_name,
            config,
        ))
    }

    #[tauri::command]
    pub fn get_action_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<ActionNodeConfig> {
        map(api::get_action_config(backend(&state), node_id))
    }

    #[tauri::command]
    pub fn update_action_node(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: ActionNodeConfig,
    ) -> CmdResult<ActionNodeConfig> {
        map(api::update_action_node(backend(&state), node_id, config))
    }

    #[tauri::command]
    pub fn test_action_node(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<ActionExecutionId> {
        map(api::test_action_node(backend(&state), node_id))
    }

    #[tauri::command]
    pub fn get_action_runtime_state(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<ActionRuntimeState> {
        map(api::get_action_runtime_state(backend(&state), node_id))
    }

    #[tauri::command]
    pub fn get_log_output_config(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<LogOutputConfig> {
        map(api::get_log_output_config(backend(&state), node_id))
    }

    #[tauri::command]
    pub fn update_log_output(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
        config: LogOutputConfig,
    ) -> CmdResult<LogOutputConfig> {
        map(api::update_log_output(backend(&state), node_id, config))
    }

    #[tauri::command]
    pub fn list_obs_connections(state: State<'_, Arc<Backend>>) -> Vec<ObsConnectionConfig> {
        api::list_obs_connections(backend(&state))
    }

    #[tauri::command]
    pub fn upsert_obs_connection(
        state: State<'_, Arc<Backend>>,
        config: ObsConnectionConfig,
    ) -> CmdResult<ObsConnectionConfig> {
        map(api::upsert_obs_connection(backend(&state), config))
    }

    #[tauri::command]
    pub fn delete_obs_connection(
        state: State<'_, Arc<Backend>>,
        id: ObsConnectionId,
    ) -> CmdResult<()> {
        map(api::delete_obs_connection(backend(&state), id))
    }

    #[tauri::command]
    pub fn set_action_secret(
        state: State<'_, Arc<Backend>>,
        id: Option<SecretId>,
        value: String,
    ) -> CmdResult<SecretId> {
        map(api::set_action_secret(backend(&state), id, value))
    }

    #[tauri::command]
    pub fn delete_action_secret(state: State<'_, Arc<Backend>>, id: SecretId) -> CmdResult<()> {
        map(api::delete_action_secret(backend(&state), id))
    }

    #[tauri::command]
    pub fn set_obs_connection_password(
        state: State<'_, Arc<Backend>>,
        connection_id: ObsConnectionId,
        password: String,
    ) -> CmdResult<ObsConnectionConfig> {
        map(api::set_obs_connection_password(
            backend(&state),
            connection_id,
            password,
        ))
    }

    #[tauri::command]
    pub fn list_node_actions(
        state: State<'_, Arc<Backend>>,
        node_id: NodeId,
    ) -> CmdResult<Vec<NodeActionDescriptor>> {
        map(api::list_node_actions(backend(&state), node_id))
    }

    #[tauri::command]
    pub fn list_shortcuts(state: State<'_, Arc<Backend>>) -> Vec<ShortcutBinding> {
        api::list_shortcuts(backend(&state))
    }

    #[tauri::command]
    pub fn create_shortcut(
        state: State<'_, Arc<Backend>>,
        request: api::CreateShortcutRequest,
    ) -> CmdResult<ShortcutBinding> {
        map(api::create_shortcut(backend(&state), request))
    }

    #[tauri::command]
    pub fn update_shortcut(
        state: State<'_, Arc<Backend>>,
        binding: ShortcutBinding,
    ) -> CmdResult<ShortcutBinding> {
        map(api::update_shortcut(backend(&state), binding))
    }

    #[tauri::command]
    pub fn delete_shortcut(state: State<'_, Arc<Backend>>, id: ShortcutId) -> CmdResult<()> {
        map(api::delete_shortcut(backend(&state), id))
    }

    #[tauri::command]
    pub fn set_shortcut_enabled(
        state: State<'_, Arc<Backend>>,
        id: ShortcutId,
        enabled: bool,
    ) -> CmdResult<ShortcutBinding> {
        map(api::set_shortcut_enabled(backend(&state), id, enabled))
    }

    #[tauri::command]
    pub fn validate_shortcut(
        state: State<'_, Arc<Backend>>,
        binding: ShortcutBinding,
    ) -> CmdResult<ShortcutValidation> {
        map(api::validate_shortcut(backend(&state), binding))
    }

    /// Forward realtime meter/state events to the frontend without command
    /// polling. Call once during Tauri setup.
    pub fn spawn_runtime_event_forwarder<R: tauri::Runtime>(
        app: tauri::AppHandle<R>,
        receiver: crossbeam_channel::Receiver<RuntimeEvent>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("tauri-audio-event-forwarder".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    let named_event = match &event {
                        RuntimeEvent::NodeMeter { node_id, meter } => {
                            Some(BackendEvent::AudioLevelChanged {
                                node_id: *node_id,
                                port_id: None,
                                meter: *meter,
                            })
                        }
                        RuntimeEvent::PortMeter {
                            node_id,
                            port_id,
                            meter,
                        } => Some(BackendEvent::AudioLevelChanged {
                            node_id: *node_id,
                            port_id: Some(*port_id),
                            meter: *meter,
                        }),
                        RuntimeEvent::NodeState { node_id, state, .. } => {
                            Some(BackendEvent::NodeRuntimeStateChanged {
                                node_id: *node_id,
                                previous_state: None,
                                state: *state,
                            })
                        }
                        RuntimeEvent::Clipping { node_id, peak } => {
                            Some(BackendEvent::AudioClipping {
                                node_id: *node_id,
                                peak: *peak,
                            })
                        }
                        RuntimeEvent::LatencyChanged {
                            node_id,
                            stage,
                            snapshot,
                            accuracy,
                        } => Some(BackendEvent::LatencyChanged {
                            node_id: *node_id,
                            stage: *stage,
                            snapshot: *snapshot,
                            accuracy: *accuracy,
                        }),
                        _ => None,
                    };
                    let _ = app.emit("audio://runtime-event", &event);
                    if let Some(named_event) = named_event {
                        let _ = app.emit(named_event.event_name(), &named_event);
                    }
                }
            })
            .expect("failed to spawn Tauri audio event forwarder")
    }

    pub fn spawn_graph_event_forwarder<R: tauri::Runtime>(
        app: tauri::AppHandle<R>,
        receiver: crossbeam_channel::Receiver<crate::event_graph::EventEnvelope>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("tauri-graph-event-forwarder".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    let _ = app.emit("backend://graph-event", &event);
                }
            })
            .expect("failed to spawn Tauri graph event forwarder")
    }

    pub fn spawn_action_event_forwarder<R: tauri::Runtime>(
        app: tauri::AppHandle<R>,
        receiver: crossbeam_channel::Receiver<ActionRuntimeEvent>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("tauri-action-event-forwarder".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    let name = match &event {
                        ActionRuntimeEvent::ExecutionStarted { .. } => "action-execution-started",
                        ActionRuntimeEvent::ExecutionFinished { .. } => "action-execution-finished",
                        ActionRuntimeEvent::ConnectionStateChanged { .. } => {
                            "action-connection-state-changed"
                        }
                    };
                    let _ = app.emit("backend://action-event", &event);
                    let _ = app.emit(name, &event);
                }
            })
            .expect("failed to spawn Tauri Action event forwarder")
    }

    pub fn spawn_shortcut_event_forwarder<R: tauri::Runtime>(
        app: tauri::AppHandle<R>,
        receiver: crossbeam_channel::Receiver<ShortcutRuntimeEvent>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("tauri-shortcut-event-forwarder".into())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    let name = match &event {
                        ShortcutRuntimeEvent::ShortcutsChanged { .. } => "shortcuts-changed",
                        ShortcutRuntimeEvent::ShortcutTriggered { .. } => "shortcut-triggered",
                        ShortcutRuntimeEvent::RegistrationStateChanged { .. } => {
                            "shortcut-registration-state-changed"
                        }
                    };
                    let _ = app.emit("backend://shortcut-event", &event);
                    let _ = app.emit(name, &event);
                }
            })
            .expect("failed to spawn Tauri Shortcut event forwarder")
    }

    /// Polls control-thread state at 10 Hz, publishes only differences, and
    /// refreshes external devices/resources outside the audio callback.
    pub fn spawn_backend_event_forwarder<R: tauri::Runtime>(
        app: tauri::AppHandle<R>,
        backend: Arc<Backend>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name("tauri-backend-event-forwarder".into())
            .spawn(move || {
                let mut monitor = BackendEventMonitor::default();
                let mut last_device_refresh = std::time::Instant::now();
                let mut last_external_refresh = std::time::Instant::now() - Duration::from_secs(1);
                loop {
                    if app.get_webview_window("main").is_none() {
                        break;
                    }
                    if last_external_refresh.elapsed() >= Duration::from_millis(500) {
                        backend.nodes.refresh_external_states();
                        last_external_refresh = std::time::Instant::now();
                    }
                    if last_device_refresh.elapsed() >= Duration::from_secs(2) {
                        let _ = api::refresh_devices(&backend);
                        last_device_refresh = std::time::Instant::now();
                    }
                    for event in monitor.poll(&backend) {
                        let _ = app.emit("backend://state-event", &event);
                        let _ = app.emit(event.event_name(), &event);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
            .expect("failed to spawn Tauri backend event forwarder")
    }

    include!(concat!(env!("OUT_DIR"), "/tauri_command_contracts.rs"));
}
