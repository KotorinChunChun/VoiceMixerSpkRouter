use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    audio::{
        buffer::AudioRingBuffer,
        engine::AudioRuntime,
        format::INTERNAL_SAMPLE_RATE,
        graph::{
            ApplicationFilterMode, CaptureMethod, NodeId, NodeProperties, NodeSpec, RuntimeState,
        },
    },
    controller::GraphController,
    edge::receiver::EdgeHub,
    error::{BackendError, Result},
    nodes::{
        input::{
            edge::EdgeInputSource,
            microphone::RingInputSource,
            replay::{ReplayPlaybackSource, ReplayTap},
            test_signal::TestSignalSource,
        },
        output::monitor::RingOutputSink,
        transform::silence_detector::{
            AtomicSilenceDetectorState, SilenceDetectorState, SilenceDetectorStateSnapshot,
        },
    },
    plugins::{instance::PluginPrepareContext, manager::PluginManager},
    recording::{
        session::ManagedRecorder,
        state::{RecorderState, RecorderStateSnapshot, RecordingOutput},
    },
    replay::{
        cache::ReplayCacheManager,
        folder_player::FolderReplayPlayer,
        player::{MemoryReplayPlayer, ReplaySource},
        state::ReplayStateSnapshot,
    },
    rvc::manager::RvcManager,
    windows::{
        devices::{resolve_endpoint_id, DeviceDirection},
        process_loopback::{start_process_loopback, ProcessCaptureHandle, ProcessLoopbackMode},
        process_watcher::{
            list_processes, normalize_exe_path, start_exe_capture,
            start_filtered_process_composite, ExeCaptureWatcher, ProcessCompositeWatcher,
            ProcessInfo,
        },
        wasapi_capture::{
            start_endpoint_loopback, start_microphone_capture_with_auto_priority, CaptureHandle,
        },
        wasapi_render::{start_render_with_exclusive, RenderHandle},
        worker_status::{DeviceWorkerSnapshot, DeviceWorkerState},
    },
};

const IO_BUFFER_SECONDS: usize = 2;
const EDGE_DISCONNECTED_HOLD: Duration = Duration::from_millis(750);
fn io_ring() -> AudioRingBuffer {
    AudioRingBuffer::new(IO_BUFFER_SECONDS * INTERNAL_SAMPLE_RATE as usize * 2)
}

fn live_input_ring() -> AudioRingBuffer {
    AudioRingBuffer::new_live_stereo(INTERNAL_SAMPLE_RATE as usize / 5)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeStartResult {
    pub node_id: NodeId,
    pub state: RuntimeState,
    pub message: Option<String>,
}

/// Runtime-owned counters only.  Values that are unavailable for a node type
/// stay `None`; callers must not infer successful audio delivery from them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeRuntimeDiagnostics {
    pub device_worker: Option<DeviceWorkerSnapshot>,
    pub received_samples: Option<u64>,
    pub dropped_samples: Option<u64>,
    pub last_error: Option<String>,
    pub composite: Option<crate::windows::process_watcher::ProcessCompositeSnapshot>,
}

// Several variants are retained solely for RAII: keeping the handle alive is
// the resource ownership contract even when no method is called on it.
#[allow(dead_code)]
enum ActiveResource {
    Capture(CaptureHandle),
    Process(ProcessCaptureHandle),
    Exe(ExeCaptureWatcher),
    Filtered(ProcessCompositeWatcher),
    Render(RenderHandle),
    Recorder(Arc<ManagedRecorder>),
    Replay(Arc<MemoryReplayPlayer>),
    FolderReplay(Arc<FolderReplayPlayer>),
    LegacyReplay(Arc<ReplayPlaybackSource>),
    TestSignal(Arc<TestSignalSource>),
    SilenceDetector(Arc<AtomicSilenceDetectorState>),
    Marker,
}

#[derive(Debug, Clone, Default)]
pub struct LatencyIoConfig {
    pub auto_capture_priority: Vec<CaptureMethod>,
    pub render_exclusive: bool,
}

/// Owns non-realtime resources associated with graph nodes. The realtime graph
/// only sees lock-free sources/sinks/processors; COM, file, process watching and
/// WebSocket work remains on dedicated workers.
pub struct NodeRuntimeManager {
    controller: Arc<GraphController>,
    runtime: Arc<AudioRuntime>,
    plugins: Arc<PluginManager>,
    rvc: Arc<RvcManager>,
    edge: EdgeHub,
    replay_caches: Arc<ReplayCacheManager>,
    active: Mutex<HashMap<NodeId, ActiveResource>>,
    edge_disconnected_since: Mutex<HashMap<NodeId, Instant>>,
    default_endpoint_bindings: Mutex<HashMap<NodeId, String>>,
    node_errors: Mutex<HashMap<NodeId, String>>,
    latency_io: Mutex<LatencyIoConfig>,
}

impl NodeRuntimeManager {
    pub fn new(
        controller: Arc<GraphController>,
        runtime: Arc<AudioRuntime>,
        plugins: Arc<PluginManager>,
        rvc: Arc<RvcManager>,
        edge: EdgeHub,
        replay_caches: Arc<ReplayCacheManager>,
    ) -> Self {
        Self {
            controller,
            runtime,
            plugins,
            rvc,
            edge,
            replay_caches,
            active: Mutex::new(HashMap::new()),
            edge_disconnected_since: Mutex::new(HashMap::new()),
            default_endpoint_bindings: Mutex::new(HashMap::new()),
            node_errors: Mutex::new(HashMap::new()),
            latency_io: Mutex::new(LatencyIoConfig::default()),
        }
    }

    pub fn set_latency_io(&self, config: LatencyIoConfig) {
        *self.latency_io.lock() = config;
    }

    pub fn any_recorder_recording(&self) -> bool {
        self.active.lock().values().any(|resource| match resource {
            ActiveResource::Recorder(recorder) => {
                recorder.snapshot().state == RecorderState::Recording
            }
            _ => false,
        })
    }

    pub fn is_active(&self, node_id: NodeId) -> bool {
        self.active.lock().contains_key(&node_id)
    }

    /// 既存の取得・再生ワーカーの音量観測口。音声デバイスを追加で開かない。
    pub fn level_probe(
        &self,
        node_id: NodeId,
    ) -> Option<Arc<crate::audio::level_probe::LevelProbe>> {
        match self.active.lock().get(&node_id) {
            Some(ActiveResource::Capture(handle)) => Some(handle.level_probe()),
            Some(ActiveResource::Render(handle)) => Some(handle.level_probe()),
            _ => None,
        }
    }

    pub fn set_node_state(&self, node_id: NodeId, state: RuntimeState) {
        self.set_state(node_id, state);
    }

    pub fn filtered_capture_status(
        &self,
        node_id: NodeId,
    ) -> Option<crate::windows::process_watcher::FilteredCaptureStatus> {
        let active = self.active.lock();
        match active.get(&node_id) {
            Some(ActiveResource::Filtered(watcher)) => Some(watcher.status().clone()),
            Some(ActiveResource::Exe(watcher)) => Some(watcher.status().clone()),
            _ => None,
        }
    }

    pub fn runtime_diagnostics(&self, node_id: NodeId) -> NodeRuntimeDiagnostics {
        let active = self.active.lock();
        let mut diagnostics = match active.get(&node_id) {
            Some(ActiveResource::Capture(handle)) => {
                let status = handle.status();
                NodeRuntimeDiagnostics {
                    device_worker: Some(status.clone()),
                    dropped_samples: Some(handle.dropped_samples()),
                    last_error: status.last_error,
                    ..Default::default()
                }
            }
            Some(ActiveResource::Process(handle)) => {
                let status = handle.status();
                NodeRuntimeDiagnostics {
                    received_samples: Some(status.received_samples),
                    dropped_samples: Some(status.dropped_samples),
                    last_error: status.last_error,
                    ..Default::default()
                }
            }
            Some(ActiveResource::Filtered(watcher)) | Some(ActiveResource::Exe(watcher)) => {
                let status = watcher.status();
                NodeRuntimeDiagnostics {
                    received_samples: Some(status.metrics.mixed_samples),
                    dropped_samples: Some(status.metrics.output_dropped_samples),
                    composite: Some(status.metrics),
                    ..Default::default()
                }
            }
            Some(ActiveResource::Render(handle)) => NodeRuntimeDiagnostics {
                device_worker: Some(handle.status()),
                ..Default::default()
            },
            _ => NodeRuntimeDiagnostics::default(),
        };
        if diagnostics.last_error.is_none() {
            diagnostics.last_error = self.node_errors.lock().get(&node_id).cloned();
        }
        diagnostics
    }

    /// Wait only for hardware endpoints. Process and Edge nodes may correctly
    /// remain idle until another application or browser starts producing audio.
    pub fn wait_for_device_workers(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let active = self.active.lock();
        for resource in active.values() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match resource {
                ActiveResource::Capture(handle) => {
                    handle.wait_until_running(remaining)?;
                }
                ActiveResource::Render(handle) => {
                    handle.wait_until_running(remaining)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn device_worker_status(&self, node_id: NodeId) -> Option<DeviceWorkerSnapshot> {
        let active = self.active.lock();
        match active.get(&node_id) {
            Some(ActiveResource::Capture(handle)) => Some(handle.status()),
            Some(ActiveResource::Render(handle)) => Some(handle.status()),
            _ => None,
        }
    }

    pub fn start_node(&self, node_id: NodeId) -> Result<NodeStartResult> {
        if self.is_active(node_id) {
            let state = self
                .controller
                .snapshot()
                .nodes
                .get(&node_id)
                .map(|node| node.state)
                .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
            return Ok(NodeStartResult {
                node_id,
                state,
                message: Some("already active".into()),
            });
        }
        let mut node = self
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        if !node.runtime_enabled {
            return Err(BackendError::InvalidOperation(
                "graph node runtime is disabled".into(),
            ));
        }
        if let NodeProperties::EndpointLoopback {
            filter,
            capture_mode,
            ..
        } = &mut node.properties
        {
            *capture_mode = if filter.mode == ApplicationFilterMode::Disabled {
                crate::audio::graph::EndpointCaptureMode::EndpointLoopback
            } else {
                crate::audio::graph::EndpointCaptureMode::FilteredProcessComposite
            };
            let _ = self.controller.update_node(node.clone());
        }
        self.set_state(node_id, RuntimeState::Starting);

        let started = self.start_spec(&node);
        match started {
            Ok(resource) => {
                self.node_errors.lock().remove(&node_id);
                let state = match &resource {
                    ActiveResource::Recorder(recorder) => {
                        runtime_state_for_recorder(recorder.snapshot().state)
                    }
                    ActiveResource::Capture(handle) => {
                        runtime_state_for_device_worker(handle.status().state)
                    }
                    ActiveResource::Render(handle) => {
                        runtime_state_for_device_worker(handle.status().state)
                    }
                    _ => match &node.properties {
                        NodeProperties::EdgeTab { stream_key, .. } => match self
                            .edge
                            .get(stream_key)
                            .map(|s| s.state(std::time::Duration::from_secs(3)))
                        {
                            Some(crate::edge::receiver::EdgeStreamState::Running) => {
                                RuntimeState::Running
                            }
                            Some(crate::edge::receiver::EdgeStreamState::Stopped) => {
                                RuntimeState::Stopped
                            }
                            _ => RuntimeState::Waiting,
                        },
                        _ => RuntimeState::Running,
                    },
                };
                self.active.lock().insert(node_id, resource);
                if let Some((selector, direction)) = default_endpoint_selector(&node.properties) {
                    if let Ok(endpoint_id) = resolve_endpoint_id(selector, direction) {
                        self.default_endpoint_bindings
                            .lock()
                            .insert(node_id, endpoint_id);
                    }
                }
                self.set_state(node_id, state);
                Ok(NodeStartResult {
                    node_id,
                    state,
                    message: None,
                })
            }
            Err(err) => {
                self.runtime.unbind_node(node_id);
                self.set_state(node_id, RuntimeState::Error);
                self.node_errors.lock().insert(node_id, err.to_string());
                Err(err)
            }
        }
    }

    fn start_spec(&self, node: &NodeSpec) -> Result<ActiveResource> {
        match &node.properties {
            NodeProperties::TestSignal { config, .. } => {
                let source = TestSignalSource::new(*config);
                self.runtime.bind_source(node.id, source.clone());
                Ok(ActiveResource::TestSignal(source))
            }
            NodeProperties::SilenceDetector { .. } => {
                let status = Arc::new(AtomicSilenceDetectorState::default());
                self.runtime.bind_silence_status(node.id, status.clone());
                Ok(ActiveResource::SilenceDetector(status))
            }
            NodeProperties::Microphone {
                endpoint_id,
                capture_method,
                ..
            } => {
                let ring = live_input_ring();
                let endpoint_id = resolve_endpoint_id(endpoint_id, DeviceDirection::Capture)?;
                let io = self.latency_io.lock().clone();
                let handle = start_microphone_capture_with_auto_priority(
                    endpoint_id,
                    *capture_method,
                    ring.clone(),
                    io.auto_capture_priority,
                )?;
                self.runtime.bind_source(
                    node.id,
                    Arc::new(RingInputSource::with_clock(ring, handle.clock())),
                );
                Ok(ActiveResource::Capture(handle))
            }
            NodeProperties::EdgeTab { stream_key, .. } => {
                // A node may be started before the browser extension connects.
                // Registering a stream here gives the extension a stable target.
                let stream = self.edge.ensure(stream_key.clone(), 48_000, 2);
                self.runtime.bind_source(
                    node.id,
                    Arc::new(EdgeInputSource {
                        jitter: stream.jitter.clone(),
                    }),
                );
                self.edge.set_receiver_enabled(stream_key, node.id, true);
                Ok(ActiveResource::Marker)
            }
            NodeProperties::ProcessPid {
                pid,
                include_process_tree,
                ..
            } => {
                if !*include_process_tree {
                    return Err(BackendError::InvalidOperation(
                        "Windows application loopback exposes include-tree or exclude-tree semantics; exact PID-only capture is not available through this API".into(),
                    ));
                }
                let ring = io_ring();
                self.runtime
                    .bind_source(node.id, Arc::new(RingInputSource::new(ring.clone())));
                Ok(ActiveResource::Process(start_process_loopback(
                    *pid,
                    ProcessLoopbackMode::IncludeTargetTree,
                    ring,
                )?))
            }
            NodeProperties::ProcessExe { exe_path, .. } => {
                let ring = io_ring();
                self.runtime
                    .bind_source(node.id, Arc::new(RingInputSource::new(ring.clone())));
                Ok(ActiveResource::Exe(start_exe_capture(
                    exe_path.clone(),
                    ring,
                )?))
            }
            NodeProperties::EndpointLoopback {
                endpoint_id,
                filter,
                ..
            } => {
                let ring = live_input_ring();
                if filter.mode == ApplicationFilterMode::Disabled {
                    let endpoint_id = resolve_endpoint_id(endpoint_id, DeviceDirection::Render)?;
                    let handle = start_endpoint_loopback(endpoint_id, ring.clone())?;
                    self.runtime.bind_source(
                        node.id,
                        Arc::new(RingInputSource::with_clock(ring, handle.clock())),
                    );
                    Ok(ActiveResource::Capture(handle))
                } else {
                    self.runtime
                        .bind_source(node.id, Arc::new(RingInputSource::new(ring.clone())));
                    Ok(ActiveResource::Filtered(start_filtered_process_composite(
                        filter.clone(),
                        ring,
                    )?))
                }
            }
            NodeProperties::FileInput {
                path,
                looped,
                loop_wait_ms,
                ..
            } => {
                let source = Arc::new(MemoryReplayPlayer::open_with_wait(
                    path,
                    *looped,
                    *loop_wait_ms,
                )?);
                self.runtime.bind_source(node.id, source.clone());
                Ok(ActiveResource::Replay(source))
            }
            NodeProperties::FolderReplay {
                path,
                mode,
                selected_file,
                ..
            } => {
                let source = Arc::new(FolderReplayPlayer::open(
                    path,
                    *mode,
                    selected_file.clone(),
                )?);
                self.runtime.bind_source(node.id, source.clone());
                Ok(ActiveResource::FolderReplay(source))
            }
            NodeProperties::ReplayInput {
                source_node_id,
                seconds,
                ..
            } => {
                let source_id = source_node_id.ok_or_else(|| {
                    BackendError::InvalidOperation("Replay input requires source_node_id".into())
                })?;
                let tap = self.runtime.get_replay_tap(source_id).unwrap_or_else(|| {
                    let tap = Arc::new(ReplayTap::new(seconds.ceil().max(1.0) as usize));
                    self.runtime.bind_replay_tap(source_id, tap.clone());
                    tap
                });
                let replay = Arc::new(ReplayPlaybackSource::from_tap(&tap, *seconds));
                self.runtime.bind_source(node.id, replay.clone());
                Ok(ActiveResource::LegacyReplay(replay))
            }
            NodeProperties::MemoryReplay {
                cache_id,
                mode,
                selected_item_id,
                ..
            } => {
                let item = match mode {
                    crate::audio::graph::MemoryReplayMode::SelectFromCache => selected_item_id
                        .map(|item_id| self.replay_caches.get_item(*cache_id, item_id))
                        .transpose()?,
                    crate::audio::graph::MemoryReplayMode::LatestFromCache => {
                        self.replay_caches.latest(*cache_id)?
                    }
                };
                let source = Arc::new(match item {
                    Some(item) => MemoryReplayPlayer::open(item.path, false)?,
                    None => MemoryReplayPlayer::new(Vec::new(), false),
                });
                self.runtime.bind_source(node.id, source.clone());
                Ok(ActiveResource::Replay(source))
            }
            NodeProperties::TransformPlugin {
                plugin_id,
                instance_id,
                parameters,
                bypass,
                switch_policy,
                ..
            } => {
                let cfg = self.runtime.settings();
                let processor = self.plugins.create_instance(
                    *instance_id,
                    plugin_id,
                    parameters.clone(),
                    PluginPrepareContext {
                        sample_rate: INTERNAL_SAMPLE_RATE,
                        channels: 2,
                        graph_block_frames: cfg.block_frames,
                    },
                )?;
                processor.set_bypass(*bypass);
                processor.set_switch_policy(*switch_policy);
                processor.start()?;
                self.runtime.bind_processor(node.id, processor);
                Ok(ActiveResource::Marker)
            }
            NodeProperties::AudioProcessingGroup { flow, .. } => {
                let cfg = self.runtime.settings();
                let mut processors = Vec::with_capacity(flow.len());
                let mut started = Vec::with_capacity(flow.len());
                for member in flow {
                    let result = (|| {
                        let processor = self.plugins.create_instance(
                            member.instance_id,
                            &member.plugin_id,
                            member.parameters.clone(),
                            PluginPrepareContext {
                                sample_rate: INTERNAL_SAMPLE_RATE,
                                channels: 2,
                                graph_block_frames: cfg.block_frames,
                            },
                        )?;
                        processor.set_bypass(member.bypass);
                        processor.set_switch_policy(member.switch_policy);
                        processor.start()?;
                        Ok::<_, crate::error::BackendError>(processor)
                    })();
                    let processor = match result {
                        Ok(processor) => processor,
                        Err(error) => {
                            for instance_id in started {
                                if let Ok(processor) = self.plugins.get(instance_id) {
                                    let _ = processor.stop();
                                }
                                self.plugins.remove(instance_id);
                            }
                            // create_instance can have inserted the failing member before start.
                            self.plugins.remove(member.instance_id);
                            return Err(error);
                        }
                    };
                    started.push(member.instance_id);
                    processors.push((member.clone(), processor));
                }
                self.runtime.bind_processor(
                    node.id,
                    crate::nodes::transform::audio_processing_group::AudioProcessingGroupProcessor::new(processors),
                );
                Ok(ActiveResource::Marker)
            }
            NodeProperties::Rvc {
                instance_id,
                bypass,
                ..
            } => {
                let processor = self.rvc.get(*instance_id)?;
                processor.set_bypass(*bypass)?;
                processor.start()?;
                self.runtime.bind_processor(node.id, processor);
                Ok(ActiveResource::Marker)
            }
            NodeProperties::MultiInput { .. }
            | NodeProperties::InputSelector { .. }
            | NodeProperties::Delay { .. }
            | NodeProperties::ChannelMapper { .. }
            | NodeProperties::Action { .. }
            | NodeProperties::LogOutput { .. } => Ok(ActiveResource::Marker),
            NodeProperties::Monitor { endpoint_id, .. }
            | NodeProperties::VirtualDevice { endpoint_id, .. } => {
                let ring = io_ring();
                let endpoint_id = resolve_endpoint_id(endpoint_id, DeviceDirection::Render)?;
                let exclusive = self.latency_io.lock().render_exclusive;
                let handle = start_render_with_exclusive(endpoint_id, ring.clone(), exclusive)?;
                self.runtime
                    .bind_sink(node.id, Arc::new(RingOutputSink::new(ring)));
                self.runtime
                    .bind_render_padding(node.id, handle.padding_frames());
                Ok(ActiveResource::Render(handle))
            }
            NodeProperties::Recorder {
                path,
                format,
                directory_mode,
                auto_record,
                ..
            } => {
                let recorder = Arc::new(if *directory_mode {
                    ManagedRecorder::folder(path, *format, *auto_record)
                } else {
                    ManagedRecorder::fixed_file(path, *format, *auto_record)
                });
                recorder.begin_session(self.recorder_input_available(node.id))?;
                self.runtime.bind_sink(node.id, recorder.clone());
                Ok(ActiveResource::Recorder(recorder))
            }
            NodeProperties::ReplayCache { cache_id, .. } => {
                let config = self.replay_caches.get_cache(*cache_id)?;
                let recorder = Arc::new(ManagedRecorder::replay_cache(
                    *cache_id,
                    crate::audio::graph::RecordingFormat::Wav,
                    self.replay_caches.clone(),
                    config.auto_record,
                ));
                recorder.begin_session(self.recorder_input_available(node.id))?;
                self.runtime.bind_sink(node.id, recorder.clone());
                Ok(ActiveResource::Recorder(recorder))
            }
        }
    }

    /// Apply new generator parameters without recreating the active source.
    pub fn update_test_signal(
        &self,
        node_id: NodeId,
        config: crate::audio::graph::TestSignalConfig,
    ) -> Result<()> {
        match self.active.lock().get(&node_id) {
            Some(ActiveResource::TestSignal(source)) => {
                source.update_config(config);
                Ok(())
            }
            Some(_) => Err(BackendError::InvalidOperation(
                "node is not a test signal input".into(),
            )),
            None => Ok(()),
        }
    }

    pub fn silence_detector_state(&self, node_id: NodeId) -> Result<SilenceDetectorStateSnapshot> {
        match self.active.lock().get(&node_id) {
            Some(ActiveResource::SilenceDetector(status)) => Ok(status.snapshot()),
            Some(_) => Err(BackendError::InvalidOperation(
                "node is not a silence detector".into(),
            )),
            None => {
                let graph = self.controller.snapshot();
                let node = graph
                    .nodes
                    .get(&node_id)
                    .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
                if node.node_type != crate::audio::graph::NodeType::SilenceDetector {
                    return Err(BackendError::InvalidOperation(
                        "node is not a silence detector".into(),
                    ));
                }
                Ok(SilenceDetectorStateSnapshot {
                    state: SilenceDetectorState::Disabled,
                    level_db: -120.0,
                    silent_duration_ms: 0,
                    dropped_events: 0,
                })
            }
        }
    }

    /// Reloads a Replay Input with the most recent N seconds from its tap.
    pub fn trigger_replay(&self, node_id: NodeId) -> Result<()> {
        let node = self
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        let NodeProperties::ReplayInput {
            source_node_id,
            seconds,
            ..
        } = node.properties
        else {
            return Err(BackendError::InvalidOperation(
                "node is not a Replay input".into(),
            ));
        };
        let source_id = source_node_id.ok_or_else(|| {
            BackendError::InvalidOperation("Replay input requires source_node_id".into())
        })?;
        let tap = self.runtime.get_replay_tap(source_id).ok_or_else(|| {
            BackendError::InvalidOperation("Replay source has no active tap yet".into())
        })?;
        let replay = Arc::new(ReplayPlaybackSource::from_tap(&tap, seconds));
        self.runtime.bind_source(node_id, replay.clone());
        self.active
            .lock()
            .insert(node_id, ActiveResource::LegacyReplay(replay));
        Ok(())
    }

    pub fn play_replay(&self, node_id: NodeId) -> Result<()> {
        if !self.is_active(node_id) {
            self.start_node(node_id)?;
        }
        let node = self
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        if let NodeProperties::MemoryReplay {
            cache_id,
            mode,
            selected_item_id,
            ..
        } = node.properties
        {
            let item = match mode {
                crate::audio::graph::MemoryReplayMode::SelectFromCache => {
                    let item_id = selected_item_id.ok_or_else(|| {
                        BackendError::InvalidOperation("no Replay Cache item is selected".into())
                    })?;
                    self.replay_caches.get_item(cache_id, item_id)?
                }
                crate::audio::graph::MemoryReplayMode::LatestFromCache => {
                    self.replay_caches.latest(cache_id)?.ok_or_else(|| {
                        BackendError::InvalidOperation("Replay Cache is empty".into())
                    })?
                }
            };
            let source = Arc::new(MemoryReplayPlayer::open(item.path, false)?);
            self.runtime.bind_source(node_id, source.clone());
            self.active
                .lock()
                .insert(node_id, ActiveResource::Replay(source));
        }
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::Replay(source)) => source.play(),
            Some(ActiveResource::FolderReplay(source)) => source.play(),
            _ => Err(BackendError::InvalidOperation(
                "node is not an active Replay source".into(),
            )),
        }
    }

    pub fn pause_replay(&self, node_id: NodeId) -> Result<()> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::Replay(source)) => source.pause(),
            Some(ActiveResource::FolderReplay(source)) => source.pause(),
            _ => Err(BackendError::InvalidOperation(
                "node is not an active Replay source".into(),
            )),
        }
    }

    pub fn stop_replay(&self, node_id: NodeId) -> Result<()> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::Replay(source)) => source.stop(),
            Some(ActiveResource::FolderReplay(source)) => source.stop(),
            Some(ActiveResource::LegacyReplay(source)) => {
                source.stop();
                Ok(())
            }
            _ => Err(BackendError::InvalidOperation(
                "node is not an active Replay source".into(),
            )),
        }
    }

    pub fn seek_replay(&self, node_id: NodeId, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(BackendError::InvalidOperation(
                "seek position must be a finite non-negative value".into(),
            ));
        }
        let guard = self.active.lock();
        let position = Duration::from_secs_f64(seconds);
        match guard.get(&node_id) {
            Some(ActiveResource::Replay(source)) => source.seek(position),
            Some(ActiveResource::FolderReplay(source)) => source.seek(position),
            _ => Err(BackendError::InvalidOperation(
                "node is not an active Replay source".into(),
            )),
        }
    }

    pub fn replay_state(&self, node_id: NodeId) -> Result<ReplayStateSnapshot> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::Replay(source)) => Ok(source.snapshot()),
            Some(ActiveResource::FolderReplay(source)) => Ok(source.snapshot()),
            _ => Err(BackendError::InvalidOperation(
                "node is not an active Replay source".into(),
            )),
        }
    }

    pub fn list_folder_files(&self, node_id: NodeId) -> Result<Vec<std::path::PathBuf>> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::FolderReplay(source)) => source.refresh_files(),
            _ => Err(BackendError::InvalidOperation(
                "folder Replay source is not active".into(),
            )),
        }
    }

    /// Returns the worker's cached folder catalog without doing filesystem I/O.
    /// The Folder Replay worker refreshes this catalog on its control thread.
    pub fn cached_folder_files(&self, node_id: NodeId) -> Result<Vec<std::path::PathBuf>> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::FolderReplay(source)) => Ok(source.files()),
            _ => Err(BackendError::InvalidOperation(
                "folder Replay source is not active".into(),
            )),
        }
    }

    pub fn select_folder_file(&self, node_id: NodeId, path: std::path::PathBuf) -> Result<()> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::FolderReplay(source)) => source.select_file(path),
            _ => Err(BackendError::InvalidOperation(
                "folder Replay source is not active".into(),
            )),
        }
    }

    pub fn set_folder_replay_mode(
        &self,
        node_id: NodeId,
        mode: crate::audio::graph::FolderReplayMode,
    ) -> Result<()> {
        let guard = self.active.lock();
        match guard.get(&node_id) {
            Some(ActiveResource::FolderReplay(source)) => source.set_mode(mode),
            _ => Err(BackendError::InvalidOperation(
                "folder Replay source is not active".into(),
            )),
        }
    }

    pub fn stop_all_replays(&self) -> usize {
        let guard = self.active.lock();
        let mut stopped = 0;
        for resource in guard.values() {
            match resource {
                ActiveResource::Replay(source) => {
                    if source.stop().is_ok() {
                        stopped += 1;
                    }
                }
                ActiveResource::FolderReplay(source) => {
                    if source.stop().is_ok() {
                        stopped += 1;
                    }
                }
                ActiveResource::LegacyReplay(source) => {
                    source.stop();
                    stopped += 1;
                }
                _ => {}
            }
        }
        stopped
    }

    pub fn start_recorder(&self, node_id: NodeId) -> Result<RecorderStateSnapshot> {
        self.start_upstream_route(node_id);
        if !self.is_active(node_id) {
            self.start_node(node_id)?;
        }
        let input_available = self.recorder_input_available(node_id);
        let recorder = self.active_recorder(node_id)?;
        recorder.start_manual(input_available)?;
        let snapshot = recorder.snapshot();
        self.set_state(node_id, runtime_state_for_recorder(snapshot.state));
        Ok(snapshot)
    }

    pub fn stop_recorder(&self, node_id: NodeId) -> Result<Option<RecordingOutput>> {
        let recorder = self.active_recorder(node_id)?;
        let output = recorder.stop_manual()?;
        let snapshot = recorder.snapshot();
        self.set_state(node_id, runtime_state_for_recorder(snapshot.state));
        Ok(output)
    }

    pub fn set_recorder_completed_output_collection(
        &self,
        node_id: NodeId,
        enabled: bool,
    ) -> Result<()> {
        self.active_recorder(node_id)?
            .set_completed_output_collection(enabled);
        Ok(())
    }

    pub fn drain_recorder_completed_outputs(
        &self,
        node_id: NodeId,
    ) -> Result<Vec<RecordingOutput>> {
        Ok(self.active_recorder(node_id)?.drain_completed_outputs())
    }

    pub fn recorder_state(&self, node_id: NodeId) -> Result<RecorderStateSnapshot> {
        if let Ok(recorder) = self.active_recorder(node_id) {
            return Ok(recorder.snapshot());
        }
        let node = self
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        let auto_record = match node.properties {
            NodeProperties::Recorder { auto_record, .. } => auto_record,
            NodeProperties::ReplayCache { cache_id, .. } => {
                self.replay_caches.get_cache(cache_id)?.auto_record
            }
            _ => {
                return Err(BackendError::InvalidOperation(
                    "node is not a recording output".into(),
                ))
            }
        };
        Ok(RecorderStateSnapshot {
            state: RecorderState::Disabled,
            auto_record,
            session_active: false,
            input_available: self.recorder_input_available(node_id),
            manual_stop_override: false,
            pending_manual_start: false,
            duration_seconds: 0.0,
            current_path: None,
            last_output: None,
            error: None,
        })
    }

    pub fn set_recorder_auto_record(&self, node_id: NodeId, auto_record: bool) -> Result<()> {
        if let Ok(recorder) = self.active_recorder(node_id) {
            recorder.set_auto_record(auto_record)?;
            let snapshot = recorder.snapshot();
            self.set_state(node_id, runtime_state_for_recorder(snapshot.state));
        }
        Ok(())
    }

    pub fn refresh_recorder_inputs(&self) -> Vec<(NodeId, Result<RecorderStateSnapshot>)> {
        let recorders: Vec<_> = {
            let active = self.active.lock();
            active
                .iter()
                .filter_map(|(node_id, resource)| match resource {
                    ActiveResource::Recorder(recorder) => Some((*node_id, recorder.clone())),
                    _ => None,
                })
                .collect()
        };
        recorders
            .into_iter()
            .map(|(node_id, recorder)| {
                // 試聴中は自動録音の開始・終了を保留する。通常配送の復帰時に再評価する。
                if self
                    .runtime
                    .audition_target()
                    .is_some_and(|target| !target.continue_outputs)
                {
                    return (node_id, Ok(recorder.snapshot()));
                }
                let result = recorder
                    .set_input_available(self.recorder_input_available(node_id))
                    .map(|_| recorder.snapshot());
                if let Ok(snapshot) = &result {
                    self.set_state(node_id, runtime_state_for_recorder(snapshot.state));
                } else {
                    self.set_state(node_id, RuntimeState::Error);
                }
                (node_id, result)
            })
            .collect()
    }

    fn active_recorder(&self, node_id: NodeId) -> Result<Arc<ManagedRecorder>> {
        match self.active.lock().get(&node_id) {
            Some(ActiveResource::Recorder(recorder)) => Ok(recorder.clone()),
            Some(_) => Err(BackendError::InvalidOperation(
                "node is not a recording output".into(),
            )),
            None => Err(BackendError::InvalidOperation(
                "recording output is not active".into(),
            )),
        }
    }

    fn recorder_input_available(&self, node_id: NodeId) -> bool {
        let graph = self.controller.snapshot();
        has_available_input_path(&graph, node_id, &mut HashSet::new())
    }

    /// Start every enabled node upstream of an output, in source-to-output
    /// order. Individual resource failures remain attached to their node and
    /// do not prevent other branches or the recorder from entering its
    /// WaitingForInput state.
    fn start_upstream_route(&self, node_id: NodeId) -> Vec<NodeStartResult> {
        let graph = self.controller.snapshot();
        let mut order = Vec::new();
        visit_upstream(&graph, node_id, &mut HashSet::new(), &mut order);
        order
            .into_iter()
            .filter(|id| *id != node_id)
            .filter_map(|id| {
                let node = graph.nodes.get(&id)?;
                if !node.enabled || !node.runtime_enabled {
                    return None;
                }
                Some(match self.start_node(id) {
                    Ok(result) => result,
                    Err(error) => NodeStartResult {
                        node_id: id,
                        state: RuntimeState::Error,
                        message: Some(error.to_string()),
                    },
                })
            })
            .collect()
    }

    pub fn pause_file(&self, node_id: NodeId) -> Result<()> {
        self.pause_replay(node_id)
    }
    pub fn resume_file(&self, node_id: NodeId) -> Result<()> {
        self.play_replay(node_id)
    }
    pub fn seek_file(&self, node_id: NodeId, seconds: f64) -> Result<()> {
        self.seek_replay(node_id, seconds)
    }
    pub fn file_position(&self, node_id: NodeId) -> Result<f64> {
        Ok(self.replay_state(node_id)?.position_seconds)
    }

    pub fn stop_node(&self, node_id: NodeId) -> Result<()> {
        let graph = self.controller.snapshot();
        let node = graph.nodes.get(&node_id).cloned();
        let resource = self.active.lock().remove(&node_id);
        // The graph mutation may have removed this NodeSpec already.  Always
        // clean the receiver registration by NodeId rather than depending on
        // the old EdgeTab properties still being present in the snapshot.
        self.edge.remove_receiver_node(node_id);
        if resource.is_none() && node.is_none() {
            return Err(BackendError::NodeNotFound(node_id.to_string()));
        }
        if let Some(node) = &node {
            match &node.properties {
                NodeProperties::TransformPlugin { instance_id, .. } => {
                    if let Ok(processor) = self.plugins.get(*instance_id) {
                        let _ = processor.stop();
                    }
                    self.plugins.remove(*instance_id);
                }
                NodeProperties::AudioProcessingGroup { flow, .. } => {
                    for member in flow {
                        if let Ok(processor) = self.plugins.get(member.instance_id) {
                            let _ = processor.stop();
                        }
                        self.plugins.remove(member.instance_id);
                    }
                }
                NodeProperties::Rvc { instance_id, .. } => {
                    if let Ok(processor) = self.rvc.get(*instance_id) {
                        let _ = processor.stop();
                    }
                }
                _ => {}
            }
        }
        if let Some(ActiveResource::Recorder(recorder)) = &resource {
            recorder.end_session()?;
        }
        // Resource drop stops worker threads before the graph binding disappears.
        drop(resource);
        self.edge_disconnected_since.lock().remove(&node_id);
        self.default_endpoint_bindings.lock().remove(&node_id);
        self.node_errors.lock().remove(&node_id);
        self.runtime.unbind_node(node_id);
        self.set_state(node_id, RuntimeState::Stopped);
        Ok(())
    }

    /// Rebuild all currently active node resources after a system resume or
    /// other global audio-stack reset. Endpoint clients/buffers and process
    /// watchers are recreated while the persisted graph itself is retained.
    pub fn recover_after_resume(&self) -> Vec<NodeStartResult> {
        let ids: Vec<NodeId> = self.active.lock().keys().copied().collect();
        for id in &ids {
            let _ = self.stop_node(*id);
        }
        ids.into_iter()
            .map(|id| match self.start_node(id) {
                Ok(x) => x,
                Err(err) => NodeStartResult {
                    node_id: id,
                    state: RuntimeState::Error,
                    message: Some(err.to_string()),
                },
            })
            .collect()
    }

    pub fn start_graph(&self) -> Vec<NodeStartResult> {
        let graph = self.controller.snapshot();
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        let mut node_ids: Vec<_> = graph.nodes.keys().copied().collect();
        node_ids.sort();
        for node_id in node_ids {
            visit_upstream(&graph, node_id, &mut visited, &mut order);
        }
        let results: Vec<_> = order
            .into_iter()
            .filter_map(|node_id| {
                graph
                    .nodes
                    .get(&node_id)
                    .filter(|node| node.enabled && node.runtime_enabled)
                    .map(|_| node_id)
            })
            .map(|node_id| match self.start_node(node_id) {
                Ok(x) => x,
                Err(err) => NodeStartResult {
                    node_id,
                    state: RuntimeState::Error,
                    message: Some(err.to_string()),
                },
            })
            .collect();
        self.refresh_recorder_inputs();
        results
    }

    pub fn stop_graph(&self) {
        let ids: Vec<NodeId> = self.active.lock().keys().copied().collect();
        for id in ids {
            let _ = self.stop_node(id);
        }
    }

    /// Refreshes resource-dependent runtime states on a control thread. This
    /// never runs in the audio processing loop and performs one process scan
    /// for all active PID/EXE nodes.
    pub fn refresh_external_states(&self) -> Vec<NodeStartResult> {
        let graph = self.controller.snapshot();
        let active_ids: HashSet<_> = self.active.lock().keys().copied().collect();
        let needs_process_scan = graph.nodes.values().any(|node| {
            active_ids.contains(&node.id)
                && matches!(
                    node.properties,
                    NodeProperties::ProcessPid { .. } | NodeProperties::ProcessExe { .. }
                )
        });
        let processes = needs_process_scan.then(list_processes).unwrap_or_default();
        self.refresh_external_states_with_processes(&processes)
    }

    /// A saved `default://` selector denotes the current Windows default, not
    /// the endpoint ID that happened to be default at startup. Recreate only
    /// the affected worker when Windows changes that mapping.
    pub fn refresh_default_endpoint_bindings(&self) -> Vec<NodeStartResult> {
        let graph = self.controller.snapshot();
        let changed: Vec<_> = graph
            .nodes
            .values()
            .filter(|node| self.is_active(node.id))
            .filter_map(|node| {
                let (selector, direction) = default_endpoint_selector(&node.properties)?;
                let resolved = match resolve_endpoint_id(selector, direction) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let message = format!("既定デバイスの解決失敗: {error}");
                        self.node_errors.lock().insert(node.id, message.clone());
                        self.set_state(node.id, RuntimeState::Error);
                        return Some(Err(NodeStartResult {
                            node_id: node.id,
                            state: RuntimeState::Error,
                            message: Some(message),
                        }));
                    }
                };
                let previous = self.default_endpoint_bindings.lock().get(&node.id).cloned();
                (previous.as_deref() != Some(resolved.as_str())).then_some(Ok(node.id))
            })
            .collect();
        changed
            .into_iter()
            .map(|changed| {
                let node_id = match changed {
                    Ok(node_id) => node_id,
                    Err(error) => return error,
                };
                if let Err(error) = self.stop_node(node_id) {
                    return NodeStartResult {
                        node_id,
                        state: RuntimeState::Error,
                        message: Some(error.to_string()),
                    };
                }
                match self.start_node(node_id) {
                    Ok(result) => result,
                    Err(error) => NodeStartResult {
                        node_id,
                        state: RuntimeState::Error,
                        message: Some(error.to_string()),
                    },
                }
            })
            .collect()
    }

    /// Deterministic variant used by tests and by the production process scan.
    pub fn refresh_external_states_with_processes(
        &self,
        processes: &[ProcessInfo],
    ) -> Vec<NodeStartResult> {
        let graph = self.controller.snapshot();
        let (active_ids, device_worker_states): (HashSet<_>, HashMap<_, _>) = {
            let active = self.active.lock();
            let ids = active.keys().copied().collect();
            let states = active
                .iter()
                .filter_map(|(node_id, resource)| match resource {
                    ActiveResource::Capture(handle) => Some((*node_id, handle.status().state)),
                    ActiveResource::Render(handle) => Some((*node_id, handle.status().state)),
                    _ => None,
                })
                .collect();
            (ids, states)
        };
        let pids: HashSet<_> = processes.iter().map(|process| process.pid).collect();
        let mut changed = Vec::new();

        for node in graph
            .nodes
            .values()
            .filter(|node| active_ids.contains(&node.id))
        {
            let next = if let Some(state) = device_worker_states.get(&node.id) {
                runtime_state_for_device_worker(*state)
            } else {
                match &node.properties {
                    NodeProperties::EdgeTab { stream_key, .. } => {
                        let stream_state = self
                            .edge
                            .get(stream_key)
                            .map(|stream| stream.state(Duration::from_secs(3)))
                            .unwrap_or(crate::edge::receiver::EdgeStreamState::Disconnected);
                        match stream_state {
                            crate::edge::receiver::EdgeStreamState::Running => {
                                self.edge_disconnected_since.lock().remove(&node.id);
                                RuntimeState::Running
                            }
                            crate::edge::receiver::EdgeStreamState::Stopped => {
                                self.edge_disconnected_since.lock().remove(&node.id);
                                RuntimeState::Stopped
                            }
                            crate::edge::receiver::EdgeStreamState::Disconnected => {
                                let mut disconnected = self.edge_disconnected_since.lock();
                                let since =
                                    disconnected.entry(node.id).or_insert_with(Instant::now);
                                if since.elapsed() >= EDGE_DISCONNECTED_HOLD {
                                    RuntimeState::Waiting
                                } else {
                                    RuntimeState::Disconnected
                                }
                            }
                            crate::edge::receiver::EdgeStreamState::Waiting
                            | crate::edge::receiver::EdgeStreamState::Timeout => {
                                RuntimeState::Waiting
                            }
                            crate::edge::receiver::EdgeStreamState::Error => RuntimeState::Error,
                        }
                    }
                    NodeProperties::ProcessPid { pid, .. } => {
                        if pids.contains(pid) {
                            RuntimeState::Running
                        } else {
                            RuntimeState::Waiting
                        }
                    }
                    NodeProperties::ProcessExe { exe_path, .. } => {
                        let expected = normalize_exe_path(exe_path);
                        if processes.iter().any(|process| {
                            process
                                .exe_path
                                .as_deref()
                                .map(normalize_exe_path)
                                .as_deref()
                                == Some(expected.as_str())
                        }) {
                            RuntimeState::Running
                        } else {
                            RuntimeState::Waiting
                        }
                    }
                    _ => continue,
                }
            };
            if node.state != next {
                self.set_state(node.id, next);
                changed.push(NodeStartResult {
                    node_id: node.id,
                    state: next,
                    message: None,
                });
            }
        }
        if !changed.is_empty() {
            self.refresh_recorder_inputs();
        }
        changed
    }

    fn set_state(&self, node_id: NodeId, state: RuntimeState) {
        let Some(mut node) = self.controller.snapshot().nodes.get(&node_id).cloned() else {
            return;
        };
        if node.state == state {
            return;
        }
        node.state = state;
        let _ = self.controller.update_node(node);
    }
}

fn visit_upstream(
    graph: &crate::audio::graph::GraphSnapshot,
    node_id: NodeId,
    visited: &mut HashSet<NodeId>,
    order: &mut Vec<NodeId>,
) {
    if !visited.insert(node_id) {
        return;
    }
    let mut sources: Vec<_> = graph.incoming(node_id).map(|edge| edge.from_node).collect();
    sources.sort();
    sources.dedup();
    for source_id in sources {
        visit_upstream(graph, source_id, visited, order);
    }
    order.push(node_id);
}

fn has_available_input_path(
    graph: &crate::audio::graph::GraphSnapshot,
    node_id: NodeId,
    visited: &mut HashSet<NodeId>,
) -> bool {
    if !visited.insert(node_id) {
        return false;
    }
    graph.incoming(node_id).any(|edge| {
        let Some(source) = graph.nodes.get(&edge.from_node) else {
            return false;
        };
        if !source.enabled
            || !source.runtime_enabled
            || !matches!(source.state, RuntimeState::Running | RuntimeState::Muted)
        {
            return false;
        }
        source.node_type.class() == crate::audio::graph::NodeClass::Input
            || has_available_input_path(graph, source.id, visited)
    })
}

fn runtime_state_for_recorder(state: RecorderState) -> RuntimeState {
    match state {
        RecorderState::Disabled => RuntimeState::Stopped,
        RecorderState::Ready => RuntimeState::Running,
        RecorderState::WaitingForInput => RuntimeState::Waiting,
        RecorderState::Recording => RuntimeState::Running,
        RecorderState::Stopping => RuntimeState::Recovering,
        RecorderState::Error => RuntimeState::Error,
    }
}

fn runtime_state_for_device_worker(state: DeviceWorkerState) -> RuntimeState {
    match state {
        DeviceWorkerState::Starting => RuntimeState::Starting,
        DeviceWorkerState::Running => RuntimeState::Running,
        DeviceWorkerState::Reconnecting => RuntimeState::Recovering,
        DeviceWorkerState::Stopped => RuntimeState::Stopped,
    }
}

fn default_endpoint_selector(props: &NodeProperties) -> Option<(&str, DeviceDirection)> {
    match props {
        NodeProperties::Microphone { endpoint_id, .. } if endpoint_id.starts_with("default://") => {
            Some((endpoint_id, DeviceDirection::Capture))
        }
        NodeProperties::Monitor { endpoint_id, .. }
        | NodeProperties::VirtualDevice { endpoint_id, .. }
            if endpoint_id.starts_with("default://") =>
        {
            Some((endpoint_id, DeviceDirection::Render))
        }
        NodeProperties::EndpointLoopback {
            endpoint_id,
            filter,
            ..
        } if filter.mode == ApplicationFilterMode::Disabled
            && endpoint_id.starts_with("default://") =>
        {
            Some((endpoint_id, DeviceDirection::Render))
        }
        _ => None,
    }
}

#[cfg(test)]
mod default_endpoint_tests {
    use super::*;
    #[test]
    fn playback_and_monitor_defaults_are_tracked_but_fixed_endpoints_are_not() {
        for kind in ["microphone", "monitor", "virtual_device"] {
            for id in [
                "default://console",
                "default://communications",
                "fixed-endpoint",
            ] {
                let props: NodeProperties = serde_json::from_value(
                    serde_json::json!({"kind":kind,"endpoint_id":id,"gain_db":0.0,"mute":false}),
                )
                .unwrap();
                let expected = if id.starts_with("default://") {
                    Some((
                        id,
                        if kind == "microphone" {
                            DeviceDirection::Capture
                        } else {
                            DeviceDirection::Render
                        },
                    ))
                } else {
                    None
                };
                assert_eq!(default_endpoint_selector(&props), expected, "{kind} {id}");
            }
        }
    }
}
