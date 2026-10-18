use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

use crate::audio::clock::{
    duration_100ns_to_us, frames_to_us, now_100ns, LatencyAccuracy, LatencyBank, LatencyMap,
    LatencySnapshot, LatencyStage, PathLatencyReport, PathLatencyStage,
};
use crate::audio::delay::DelayLine;
use crate::audio::format::{AudioBlock, INTERNAL_SAMPLE_RATE};
use crate::audio::graph::{
    GraphSnapshot, NodeClass, NodeId, NodeProperties, NodeType, PortId, RuntimeState,
    TransformFailurePolicy,
};
use crate::audio::meter::{measure, MeterFrame};
use crate::error::{BackendError, Result};
use crate::event_graph::{EventIngress, PendingEvent};
use crate::nodes::input::replay::ReplayTap;
use crate::nodes::transform::silence_detector::{
    rms_db, AtomicSilenceDetectorState, SilenceDetectorProcessor,
};
use crate::nodes::transform::{channel_mapper, delay::DelayProcessor};
use crate::replay::state::ReplayStateSnapshot;

pub trait AudioSource: Send + Sync {
    /// Must not wait on network/file/UI operations. Implementations should read
    /// from a pre-filled ring/jitter buffer and return silence on underflow.
    fn read_into(&self, block: &mut AudioBlock) -> Result<()>;

    /// 前回報告からライブ入力が遅延回復のために破棄したサンプル数。
    fn take_discarded_samples(&self) -> u64 {
        0
    }

    /// Returns replay progress when this source is a replay player. Live and
    /// process-backed sources use the default `None`.
    fn replay_snapshot(&self) -> Option<ReplayStateSnapshot> {
        None
    }
}

pub trait AudioSink: Send + Sync {
    /// Must be non-blocking from the graph thread. Hardware/file sinks should
    /// enqueue into their dedicated writer/render thread.
    fn write(&self, block: &AudioBlock) -> Result<()>;
    /// 試聴開始時に既に待機中の通常出力を破棄できるsinkはここで処理する。
    fn pause(&self) {}
}

pub trait AudioProcessor: Send + Sync {
    fn process_in_place(&self, block: &mut AudioBlock) -> Result<()>;
    fn set_connected_ports(&self, _connected: &[String]) -> Result<()> {
        Ok(())
    }
    /// Processes a node with explicit per-port buffers. Legacy processors keep
    /// the one-in/one-out behavior through this default adapter.
    fn process_ports(&self, inputs: &mut [AudioBlock], outputs: &mut [AudioBlock]) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        if let Some(input) = inputs.first() {
            outputs[0].samples.copy_from_slice(&input.samples);
            outputs[0].copy_latency_from(input);
        } else {
            outputs[0].clear();
        }
        self.process_in_place(&mut outputs[0])
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn queued_frames(&self) -> u32 {
        0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub block_frames: usize,
    pub meter_fps: u32,
}

impl RuntimeSettings {
    pub fn validate(&self) -> Result<()> {
        if !(64..=4096).contains(&self.block_frames) {
            return Err(BackendError::InvalidOperation(
                "block_frames must be between 64 and 4096".into(),
            ));
        }
        if !(1..=120).contains(&self.meter_fps) {
            return Err(BackendError::InvalidOperation(
                "meter_fps must be between 1 and 120".into(),
            ));
        }
        Ok(())
    }
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            block_frames: 480,
            meter_fps: 30,
        }
    }
}

#[derive(Clone, Default)]
struct RuntimeBindings {
    revision: u64,
    sources: HashMap<NodeId, Arc<dyn AudioSource>>,
    sinks: HashMap<NodeId, Arc<dyn AudioSink>>,
    processors: HashMap<NodeId, Arc<dyn AudioProcessor>>,
    replay_taps: HashMap<NodeId, Arc<ReplayTap>>,
    event_ingress: Option<EventIngress>,
    silence_statuses: HashMap<NodeId, Arc<AtomicSilenceDetectorState>>,
    render_padding: HashMap<NodeId, Arc<AtomicU32>>,
}

impl RuntimeBindings {
    fn bumped(mut self) -> Self {
        self.revision = self.revision.saturating_add(1);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeEvent {
    NodeState {
        node_id: NodeId,
        state: RuntimeState,
        message: Option<String>,
    },
    NodeMeter {
        node_id: NodeId,
        meter: MeterFrame,
    },
    PortMeter {
        node_id: NodeId,
        port_id: PortId,
        meter: MeterFrame,
    },
    Clipping {
        node_id: NodeId,
        peak: f32,
    },
    GraphRevisionApplied {
        revision: u64,
    },
    RuntimeSettingsApplied {
        block_frames: usize,
        meter_fps: u32,
    },
    Xrun {
        node_id: Option<NodeId>,
        message: String,
    },
    LatencyChanged {
        node_id: NodeId,
        stage: LatencyStage,
        snapshot: LatencySnapshot,
        accuracy: LatencyAccuracy,
    },
}

pub struct AudioRuntime {
    switcher_statuses: Arc<
        parking_lot::Mutex<
            HashMap<NodeId, crate::nodes::transform::input_selector::SwitcherSnapshot>,
        >,
    >,
    audition: Arc<ArcSwapOption<crate::audio::audition::AuditionSession>>,
    graph: Arc<ArcSwap<GraphSnapshot>>,
    bindings: Arc<ArcSwap<RuntimeBindings>>,
    settings: Arc<ArcSwap<RuntimeSettings>>,
    latency: Arc<ArcSwap<LatencyMap>>,
    events_tx: Sender<RuntimeEvent>,
    events_rx: Receiver<RuntimeEvent>,
    running: Arc<AtomicBool>,
}

impl AudioRuntime {
    pub fn new(graph: Arc<ArcSwap<GraphSnapshot>>, settings: RuntimeSettings) -> Self {
        let (events_tx, events_rx) = crossbeam_channel::bounded(2048);
        Self {
            switcher_statuses: Arc::default(),
            audition: Arc::new(ArcSwapOption::empty()),
            graph,
            bindings: Arc::new(ArcSwap::from_pointee(RuntimeBindings::default())),
            settings: Arc::new(ArcSwap::from_pointee(settings)),
            latency: Arc::new(ArcSwap::from_pointee(LatencyMap::default())),
            events_tx,
            events_rx,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn event_receiver(&self) -> Receiver<RuntimeEvent> {
        self.events_rx.clone()
    }

    pub fn settings(&self) -> RuntimeSettings {
        self.settings.load_full().as_ref().clone()
    }

    pub fn set_audition(&self, session: Option<crate::audio::audition::AuditionSession>) {
        self.audition.store(session.map(Arc::new));
    }

    pub fn audition_target(&self) -> Option<crate::audio::audition::AuditionTarget> {
        self.audition
            .load()
            .as_ref()
            .map(|session| session.target.clone())
    }
    pub fn audition_comparison(
        &self,
    ) -> Option<Arc<parking_lot::Mutex<crate::audio::comparison::ComparisonMeter>>> {
        self.audition
            .load()
            .as_ref()
            .map(|session| session.comparison.clone())
    }
    pub fn switcher_status(
        &self,
        id: NodeId,
    ) -> crate::nodes::transform::input_selector::SwitcherSnapshot {
        self.switcher_statuses
            .lock()
            .get(&id)
            .copied()
            .unwrap_or_default()
    }

    /// ランタイム稼働中に追加されたノードを、制御側で直ちに開始するための状態。
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// May be called while running. A changed block size is applied at the next
    /// safe graph-block boundary; buffers and latency compensation are rebuilt
    /// without mutating the active plan in place.
    pub fn update_settings(&self, settings: RuntimeSettings) -> Result<()> {
        settings.validate()?;
        self.settings.store(Arc::new(settings));
        Ok(())
    }

    pub fn bind_source(&self, node_id: NodeId, source: Arc<dyn AudioSource>) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.sources.insert(node_id, source);
        self.bindings.store(Arc::new(next));
    }

    pub fn bind_sink(&self, node_id: NodeId, sink: Arc<dyn AudioSink>) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.sinks.insert(node_id, sink);
        self.bindings.store(Arc::new(next));
    }

    pub fn bind_processor(&self, node_id: NodeId, processor: Arc<dyn AudioProcessor>) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.processors.insert(node_id, processor);
        self.bindings.store(Arc::new(next));
    }

    pub fn bind_replay_tap(&self, node_id: NodeId, tap: Arc<ReplayTap>) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.replay_taps.insert(node_id, tap);
        self.bindings.store(Arc::new(next));
    }

    pub fn bind_event_ingress(&self, ingress: EventIngress) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.event_ingress = Some(ingress);
        self.bindings.store(Arc::new(next));
    }

    pub fn bind_silence_status(&self, node_id: NodeId, status: Arc<AtomicSilenceDetectorState>) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.silence_statuses.insert(node_id, status);
        self.bindings.store(Arc::new(next));
    }

    pub fn bind_render_padding(&self, node_id: NodeId, padding: Arc<AtomicU32>) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.render_padding.insert(node_id, padding);
        self.bindings.store(Arc::new(next));
    }

    pub fn latency_report(&self) -> Vec<PathLatencyReport> {
        path_latency_reports(&self.graph.load(), &self.latency.load())
    }

    pub fn get_replay_tap(&self, node_id: NodeId) -> Option<Arc<ReplayTap>> {
        self.bindings.load().replay_taps.get(&node_id).cloned()
    }

    pub fn unbind_node(&self, node_id: NodeId) {
        let mut next = self.bindings.load_full().as_ref().clone().bumped();
        next.sources.remove(&node_id);
        next.sinks.remove(&node_id);
        next.processors.remove(&node_id);
        next.replay_taps.remove(&node_id);
        next.silence_statuses.remove(&node_id);
        next.render_padding.remove(&node_id);
        self.bindings.store(Arc::new(next));
    }

    pub fn start(&self) -> Result<RuntimeHandle> {
        let graph_snapshot = self.graph.load();
        let report = crate::routing::validator::validate_graph(&graph_snapshot);
        if !report.is_valid() {
            let message = report
                .issues
                .iter()
                .filter(|issue| {
                    issue.severity == crate::routing::validator::ValidationSeverity::Error
                })
                .map(|issue| issue.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(BackendError::GraphValidation(message));
        }
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(BackendError::InvalidOperation(
                "audio runtime already running".into(),
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let graph = self.graph.clone();
        let bindings = self.bindings.clone();
        let settings = self.settings.clone();
        let latency = self.latency.clone();
        let tx = self.events_tx.clone();
        let running = self.running.clone();
        let audition = self.audition.clone();
        let switcher_statuses = self.switcher_statuses.clone();

        let join = thread::Builder::new()
            .name("audio-graph-runtime".into())
            .spawn(move || {
                let mut plan = RuntimePlan::empty();
                let mut applied_settings = RuntimeSettings::default();
                let mut next_meter = Instant::now();
                let mut was_auditioning = false;
                let mut pacer = crate::audio::pacing::BlockPacer::new(
                    Instant::now(),
                    Duration::from_secs_f64(settings.load().block_frames as f64 / 48_000.0),
                );

                while !stop_thread.load(Ordering::Acquire) {
                    let started = Instant::now();
                    let cfg = settings.load_full();
                    let block_duration =
                        Duration::from_secs_f64(cfg.block_frames as f64 / 48_000.0);
                    let meter_duration = Duration::from_secs_f64(1.0 / cfg.meter_fps.max(1) as f64);
                    let graph_guard = graph.load();
                    let bindings_guard = bindings.load();

                    if plan.graph_revision != graph_guard.revision
                        || plan.bindings_revision != bindings_guard.revision
                        || plan.frames != cfg.block_frames
                    {
                        let user_delays = std::mem::take(&mut plan.user_delays);
                        let silence_detectors = std::mem::take(&mut plan.silence_detectors);
                        let mut switchers = std::mem::take(&mut plan.switchers);
                        plan = RuntimePlan::compile(
                            &graph_guard,
                            &bindings_guard,
                            &latency,
                            cfg.block_frames,
                            user_delays,
                            silence_detectors,
                        );
                        for (id, (state, ports, _)) in &mut plan.switchers {
                            if let Some((previous, _, _)) = switchers.remove(id) {
                                *state = previous;
                                state.prepare_ports(ports);
                            }
                        }
                        for (node_id, message) in &plan.compile_errors {
                            let _ = tx.try_send(RuntimeEvent::NodeState {
                                node_id: *node_id,
                                state: RuntimeState::Error,
                                message: Some(message.clone()),
                            });
                        }
                        let _ = tx.try_send(RuntimeEvent::GraphRevisionApplied {
                            revision: graph_guard.revision,
                        });
                    }

                    if *cfg != applied_settings {
                        applied_settings = cfg.as_ref().clone();
                        let _ = tx.try_send(RuntimeEvent::RuntimeSettingsApplied {
                            block_frames: applied_settings.block_frames,
                            meter_fps: applied_settings.meter_fps,
                        });
                    }

                    let now = Instant::now();
                    let emit_meters = now >= next_meter;
                    if emit_meters {
                        // Advance from the planned deadline instead of `now`.
                        // Otherwise a 10 ms graph block turns 30 fps into 25 fps
                        // because every 33.3 ms deadline is rounded up to 40 ms.
                        while next_meter <= now {
                            next_meter += meter_duration;
                        }
                    }
                    let preview = audition.load_full();
                    let suspend_outputs = preview
                        .as_ref()
                        .is_some_and(|session| !session.target.continue_outputs);
                    if suspend_outputs && !was_auditioning {
                        for sink in bindings_guard.sinks.values() {
                            sink.pause();
                        }
                    }
                    was_auditioning = suspend_outputs;
                    plan.process_block(
                        &graph_guard,
                        &bindings_guard,
                        &tx,
                        emit_meters,
                        preview.as_deref(),
                    );
                    if preview
                        .as_ref()
                        .is_some_and(|session| session.failed.load(Ordering::Acquire))
                    {
                        audition.store(None);
                    }
                    if emit_meters {
                        if let Some(mut statuses) = switcher_statuses.try_lock() {
                            statuses.retain(|id, _| plan.switchers.contains_key(id));
                            for (id, (state, _, _)) in &plan.switchers {
                                statuses.insert(*id, state.snapshot());
                            }
                        }
                    }

                    let elapsed = started.elapsed();
                    let (wait, stalled) = pacer.finish_block(Instant::now(), block_duration);
                    if stalled || elapsed > block_duration + Duration::from_millis(2) {
                        let _ = tx.try_send(RuntimeEvent::Xrun {
                            node_id: None,
                            message: format!(
                                "graph block took {:?}, budget {:?}, scheduler stalled={stalled}",
                                elapsed, block_duration,
                            ),
                        });
                    }
                    if !wait.is_zero() {
                        thread::sleep(wait);
                    }
                }
                running.store(false, Ordering::Release);
            })?;

        Ok(RuntimeHandle {
            stop,
            join: Some(join),
        })
    }
}

pub struct RuntimeHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl RuntimeHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct IncomingRef {
    source_index: usize,
    source_port_index: usize,
    to_port: PortId,
    delay_index: usize,
    gain_db: f32,
    mute: bool,
}

struct RuntimePlan {
    audition_block: AudioBlock,
    comparison_raw: AudioBlock,
    comparison_aligned: AudioBlock,
    comparison_delays: HashMap<NodeId, DelayLine>,
    switchers: HashMap<
        NodeId,
        (
            crate::nodes::transform::input_selector::SwitcherRuntime,
            Vec<PortId>,
            Vec<(PortId, f32)>,
        ),
    >,
    graph_revision: u64,
    bindings_revision: u64,
    frames: usize,
    order: Vec<NodeId>,
    incoming: Vec<Vec<IncomingRef>>,
    current: Vec<AudioBlock>,
    previous: Vec<AudioBlock>,
    current_ports: Vec<Vec<AudioBlock>>,
    previous_ports: Vec<Vec<AudioBlock>>,
    plugin_inputs: Vec<AudioBlock>,
    plugin_outputs: Vec<AudioBlock>,
    backup: Vec<AudioBlock>,
    delays: Vec<DelayLine>,
    user_delays: HashMap<NodeId, DelayProcessor>,
    silence_detectors: HashMap<NodeId, SilenceDetectorProcessor>,
    latency_banks: Vec<Option<Arc<LatencyBank>>>,
    compile_errors: Vec<(NodeId, String)>,
}

impl RuntimePlan {
    fn empty() -> Self {
        Self {
            audition_block: AudioBlock::silence(0),
            comparison_raw: AudioBlock::silence(0),
            comparison_aligned: AudioBlock::silence(0),
            comparison_delays: HashMap::new(),
            switchers: HashMap::new(),
            graph_revision: u64::MAX,
            bindings_revision: u64::MAX,
            frames: 0,
            order: Vec::new(),
            incoming: Vec::new(),
            current: Vec::new(),
            previous: Vec::new(),
            current_ports: Vec::new(),
            previous_ports: Vec::new(),
            plugin_inputs: Vec::new(),
            plugin_outputs: Vec::new(),
            backup: Vec::new(),
            delays: Vec::new(),
            user_delays: HashMap::new(),
            silence_detectors: HashMap::new(),
            latency_banks: Vec::new(),
            compile_errors: Vec::new(),
        }
    }

    fn compile(
        graph: &GraphSnapshot,
        bindings: &RuntimeBindings,
        latency: &Arc<ArcSwap<LatencyMap>>,
        frames: usize,
        mut user_delays: HashMap<NodeId, DelayProcessor>,
        mut silence_detectors: HashMap<NodeId, SilenceDetectorProcessor>,
    ) -> Self {
        // Event-only Action/Log nodes are intentionally absent from the DSP
        // plan. Event edges are consumed by EventEngine/ActionEngine and must
        // never add per-block work or meters to the realtime audio callback.
        let mut indegree: HashMap<NodeId, usize> = graph
            .nodes
            .values()
            .filter(|node| {
                node.input_ports
                    .iter()
                    .chain(&node.output_ports)
                    .any(|port| port.signal_kind == crate::audio::graph::SignalKind::Audio)
            })
            .map(|node| (node.id, 0))
            .collect();
        let mut outgoing: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for edge in &graph.edges {
            if let Some(degree) = indegree.get_mut(&edge.to_node) {
                *degree += 1;
            }
            if indegree.contains_key(&edge.from_node) && indegree.contains_key(&edge.to_node) {
                outgoing
                    .entry(edge.from_node)
                    .or_default()
                    .push(edge.to_node);
            }
        }
        let mut queue: VecDeque<NodeId> = indegree
            .iter()
            .filter_map(|(&n, &d)| (d == 0).then_some(n))
            .collect();
        let mut order = Vec::with_capacity(graph.nodes.len());
        while let Some(n) = queue.pop_front() {
            order.push(n);
            if let Some(nexts) = outgoing.get(&n) {
                for &m in nexts {
                    if let Some(d) = indegree.get_mut(&m) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push_back(m);
                        }
                    }
                }
            }
        }
        // Cycles are warnings, not hard errors. Back/cyclic edges read the
        // previous block, preventing recursive execution while preserving the
        // user's requested route for diagnostics.
        let mut leftovers: Vec<NodeId> = indegree
            .keys()
            .copied()
            .filter(|n| !order.contains(n))
            .collect();
        leftovers.sort();
        order.extend(leftovers);

        let index_by_node: HashMap<NodeId, usize> =
            order.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        let mut output_latency = vec![0usize; order.len()];
        for idx in 0..order.len() {
            let node_id = order[idx];
            let max_input = graph
                .incoming(node_id)
                .filter_map(|e| {
                    index_by_node
                        .get(&e.from_node)
                        .copied()
                        .filter(|src| *src < idx)
                        .map(|src| output_latency[src])
                })
                .max()
                .unwrap_or(0);
            let own = bindings
                .processors
                .get(&node_id)
                .map(|p| p.latency_frames() as usize)
                .unwrap_or(0);
            output_latency[idx] = max_input.saturating_add(own);
        }

        let mut incoming = vec![Vec::new(); order.len()];
        let mut delays = Vec::new();
        for dst in 0..order.len() {
            let node_id = order[dst];
            let incoming_edges: Vec<_> = graph.incoming(node_id).collect();
            let target_max = incoming_edges
                .iter()
                .filter_map(|e| {
                    index_by_node
                        .get(&e.from_node)
                        .copied()
                        .filter(|src| *src < dst)
                        .map(|src| output_latency[src])
                })
                .max()
                .unwrap_or(0);
            for edge in incoming_edges {
                if let Some(&src) = index_by_node.get(&edge.from_node) {
                    let delay_frames = if graph
                        .nodes
                        .get(&node_id)
                        .map(|n| n.node_type == NodeType::MultiInput)
                        .unwrap_or(false)
                        && src < dst
                    {
                        target_max.saturating_sub(output_latency[src])
                    } else {
                        0
                    };
                    let delay_index = delays.len();
                    delays.push(DelayLine::new(delay_frames, 2));
                    incoming[dst].push(IncomingRef {
                        source_index: src,
                        source_port_index: graph
                            .nodes
                            .get(&edge.from_node)
                            .and_then(|node| {
                                node.output_ports
                                    .iter()
                                    .position(|port| port.id == edge.from_port)
                            })
                            .unwrap_or(0),
                        to_port: edge.to_port,
                        delay_index,
                        gain_db: edge.gain_db,
                        mute: edge.mute,
                    });
                }
            }
        }

        let blocks = order.len();
        let port_counts: Vec<(usize, usize)> = order
            .iter()
            .map(|node_id| {
                graph
                    .nodes
                    .get(node_id)
                    .map(|node| {
                        (
                            node.input_ports
                                .iter()
                                .filter(|port| {
                                    port.signal_kind == crate::audio::graph::SignalKind::Audio
                                })
                                .count(),
                            node.output_ports
                                .iter()
                                .filter(|port| {
                                    port.signal_kind == crate::audio::graph::SignalKind::Audio
                                })
                                .count(),
                        )
                    })
                    .unwrap_or((0, 0))
            })
            .collect();
        let max_input_ports = port_counts
            .iter()
            .map(|(inputs, _)| *inputs)
            .max()
            .unwrap_or(1);
        let max_output_ports = port_counts
            .iter()
            .map(|(_, outputs)| *outputs)
            .max()
            .unwrap_or(1);
        let mut compile_errors = Vec::new();
        // 無効なプラグインには通知せず、背景での解放・準備を待たない。
        // 実行計画は音声スレッドで更新するため、接続不変時はprocessor側で省略する。
        for node in graph.nodes.values().filter(|node| {
            node.node_type == NodeType::TransformPlugin && node.enabled && node.runtime_enabled
        }) {
            if let Some(processor) = bindings.processors.get(&node.id) {
                if let Err(error) = connected_process_plugin_ports(node, graph)
                    .and_then(|connected| processor.set_connected_ports(&connected))
                {
                    compile_errors
                        .push((node.id, format!("transform plugin Port通知エラー: {error}")));
                }
            }
        }
        user_delays.retain(|node_id, _| {
            graph
                .nodes
                .get(node_id)
                .is_some_and(|node| node.node_type == NodeType::Delay)
        });
        silence_detectors.retain(|node_id, _| {
            graph
                .nodes
                .get(node_id)
                .is_some_and(|node| node.node_type == NodeType::SilenceDetector)
        });
        for node in graph.nodes.values() {
            if node.node_type == NodeType::Delay {
                user_delays.entry(node.id).or_insert_with(|| {
                    DelayProcessor::new(
                        crate::audio::format::INTERNAL_SAMPLE_RATE,
                        crate::audio::format::INTERNAL_CHANNELS,
                    )
                });
            }
            if node.node_type == NodeType::SilenceDetector {
                silence_detectors.entry(node.id).or_default();
            }
        }
        let mut next_map = (*latency.load_full()).clone();
        let mut latency_banks = Vec::with_capacity(order.len());
        for &node_id in &order {
            let bank = next_map
                .entry(node_id)
                .or_insert_with(|| Arc::new(LatencyBank::default()))
                .clone();
            latency_banks.push(Some(bank));
        }
        latency.store(Arc::new(next_map));
        let switchers = graph
            .nodes
            .values()
            .filter(|node| node.node_type == NodeType::InputSelector)
            .map(|node| {
                let ports: Vec<_> = node.input_ports.iter().map(|port| port.id).collect();
                let mut state = crate::nodes::transform::input_selector::SwitcherRuntime::default();
                state.prepare_ports(&ports);
                (
                    node.id,
                    (state, ports.clone(), Vec::with_capacity(ports.len())),
                )
            })
            .collect();
        Self {
            audition_block: AudioBlock::silence(frames),
            comparison_raw: AudioBlock::silence(frames),
            comparison_aligned: AudioBlock::silence(frames),
            comparison_delays: bindings
                .processors
                .iter()
                .map(|(id, processor)| {
                    (*id, DelayLine::new(processor.latency_frames() as usize, 2))
                })
                .collect(),
            switchers,
            graph_revision: graph.revision,
            bindings_revision: bindings.revision,
            frames,
            order,
            incoming,
            current: (0..blocks).map(|_| AudioBlock::silence(frames)).collect(),
            previous: (0..blocks).map(|_| AudioBlock::silence(frames)).collect(),
            current_ports: port_counts
                .iter()
                .map(|(_, outputs)| {
                    (0..(*outputs).max(1))
                        .map(|_| AudioBlock::silence(frames))
                        .collect()
                })
                .collect(),
            previous_ports: port_counts
                .iter()
                .map(|(_, outputs)| {
                    (0..(*outputs).max(1))
                        .map(|_| AudioBlock::silence(frames))
                        .collect()
                })
                .collect(),
            plugin_inputs: (0..max_input_ports.max(2))
                .map(|_| AudioBlock::silence(frames))
                .collect(),
            plugin_outputs: (0..max_output_ports.max(1))
                .map(|_| AudioBlock::silence(frames))
                .collect(),
            backup: (0..blocks).map(|_| AudioBlock::silence(frames)).collect(),
            delays,
            user_delays,
            silence_detectors,
            latency_banks,
            compile_errors,
        }
    }

    fn process_block(
        &mut self,
        graph: &GraphSnapshot,
        bindings: &RuntimeBindings,
        tx: &Sender<RuntimeEvent>,
        emit_meters: bool,
        audition: Option<&crate::audio::audition::AuditionSession>,
    ) {
        let block_now = now_100ns();
        for idx in 0..self.order.len() {
            self.current[idx].clear();
            for port in &mut self.current_ports[idx] {
                port.clear();
            }
            let node_id = self.order[idx];
            let Some(node) = graph.nodes.get(&node_id) else {
                continue;
            };
            if !node.enabled || !node.runtime_enabled {
                continue;
            }
            let bank = self.latency_banks.get(idx).and_then(|slot| slot.clone());
            let started = Instant::now();

            let result = match node.node_type.class() {
                NodeClass::Input => {
                    let read = if let Some(source) = bindings.sources.get(&node_id) {
                        let result = source.read_into(&mut self.current[idx]);
                        if emit_meters {
                            let discarded = source.take_discarded_samples();
                            if discarded > 0 {
                                let _ = tx.try_send(RuntimeEvent::Xrun {
                                    node_id: Some(node_id),
                                    message: format!(
                                        "ライブ入力の滞留を解消: {}フレームを破棄",
                                        discarded / 2
                                    ),
                                });
                            }
                        }
                        result
                    } else {
                        self.current[idx].clear();
                        Ok(())
                    };
                    if self.current[idx].capture_time_100ns.is_none() {
                        self.current[idx].stamp_host_now();
                    }
                    if let (Some(bank), Some(captured)) =
                        (bank.as_ref(), self.current[idx].capture_time_100ns)
                    {
                        bank.capture
                            .record_us(duration_100ns_to_us(block_now.saturating_sub(captured)));
                        bank.set_accuracy(self.current[idx].accuracy);
                    }
                    for output in self.current_ports[idx].iter_mut() {
                        output.samples.copy_from_slice(&self.current[idx].samples);
                        output.copy_latency_from(&self.current[idx]);
                    }
                    read
                }
                NodeClass::Transform if node.node_type == NodeType::MultiInput => {
                    self.mix_multi_input(idx, node, tx, emit_meters);
                    let meter = measure(&self.current[idx].samples);
                    if meter.peak > 1.0 {
                        let _ = tx.try_send(RuntimeEvent::Clipping {
                            node_id,
                            peak: meter.peak,
                        });
                    }
                    Ok(())
                }
                NodeClass::Transform if node.node_type == NodeType::InputSelector => {
                    self.copy_selected_input(idx, node);
                    Ok(())
                }
                NodeClass::Transform if node.node_type == NodeType::Delay => {
                    self.copy_first_input(idx);
                    if let NodeProperties::Delay { config, .. } = node.properties {
                        if let Some(processor) = self.user_delays.get_mut(&node_id) {
                            processor.process_in_place(&mut self.current[idx], config);
                        }
                        let delay_frames = (config.delay_ms
                            * self.current[idx].format.sample_rate as f32
                            / 1_000.0)
                            .round() as usize;
                        self.current[idx].shift_capture_time(delay_frames);
                    }
                    Ok(())
                }
                NodeClass::Transform if node.node_type == NodeType::ChannelMapper => {
                    if let NodeProperties::ChannelMapper { ref config, .. } = node.properties {
                        let count = node.input_ports.len().min(2);
                        for (port_index, port) in node.input_ports.iter().take(2).enumerate() {
                            self.plugin_inputs[port_index].clear();
                            for input in self.incoming[idx]
                                .iter()
                                .filter(|input| input.to_port == port.id && !input.mute)
                            {
                                let source = if input.source_index < idx {
                                    &self.current_ports[input.source_index]
                                } else {
                                    &self.previous_ports[input.source_index]
                                };
                                if let Some(block) = source.get(input.source_port_index) {
                                    let gain = 10.0_f32.powf(input.gain_db / 20.0);
                                    self.plugin_inputs[port_index].copy_latency_from(block);
                                    for (output, value) in self.plugin_inputs[port_index]
                                        .samples
                                        .iter_mut()
                                        .zip(&block.samples)
                                    {
                                        *output += value * gain;
                                    }
                                }
                            }
                        }
                        if count == 2 {
                            let ports = [node.input_ports[0].id, node.input_ports[1].id];
                            channel_mapper::process_ports(
                                &[
                                    (ports[0], &self.plugin_inputs[0]),
                                    (ports[1], &self.plugin_inputs[1]),
                                ],
                                &ports,
                                &mut self.current[idx],
                                config,
                            );
                        } else if count == 1 {
                            let ports = [node.input_ports[0].id];
                            channel_mapper::process_ports(
                                &[(ports[0], &self.plugin_inputs[0])],
                                &ports,
                                &mut self.current[idx],
                                config,
                            );
                        }
                    }
                    Ok(())
                }
                NodeClass::Transform if node.node_type == NodeType::SilenceDetector => {
                    self.copy_first_input(idx);
                    if let NodeProperties::SilenceDetector { config } = node.properties {
                        let level_db = rms_db(&self.current[idx].samples);
                        if let Some(processor) = self.silence_detectors.get_mut(&node_id) {
                            let transition = processor.process(&self.current[idx], config);
                            if let Some(status) = bindings.silence_statuses.get(&node_id) {
                                let snapshot = processor
                                    .snapshot(level_db, self.current[idx].format.sample_rate);
                                status.store(
                                    snapshot.state,
                                    snapshot.level_db,
                                    snapshot.silent_duration_ms,
                                );
                            }
                            if let Some(transition) = transition {
                                let output_port = node
                                    .output_ports
                                    .iter()
                                    .find(|port| {
                                        port.signal_kind == crate::audio::graph::SignalKind::Event
                                    })
                                    .map(|port| port.id);
                                let accepted = output_port
                                    .zip(bindings.event_ingress.as_ref())
                                    .is_some_and(|(source_port_id, ingress)| {
                                        ingress.try_send(PendingEvent::Silence {
                                            source_node_id: node_id,
                                            source_port_id,
                                            kind: transition.kind,
                                            level_db: transition.level_db,
                                            silent_duration_ms: transition.silent_duration_ms,
                                        })
                                    });
                                if !accepted {
                                    if let Some(status) = bindings.silence_statuses.get(&node_id) {
                                        status.increment_dropped();
                                    }
                                }
                            }
                        }
                    }
                    Ok(())
                }
                NodeClass::Transform if node.node_type == NodeType::AudioProcessingGroup => {
                    self.copy_first_input(idx);
                    if let Some(processor) = bindings.processors.get(&node_id) {
                        processor
                            .process_in_place(&mut self.current[idx])
                            .map(|()| {
                                self.current[idx]
                                    .shift_capture_time(processor.latency_frames() as usize);
                            })
                    } else {
                        Ok(())
                    }
                }
                NodeClass::Transform if node.node_type == NodeType::TransformPlugin => {
                    let NodeProperties::TransformPlugin {
                        bypass,
                        failure_policy,
                        gain_db,
                        mute,
                        ..
                    } = &node.properties
                    else {
                        unreachable!()
                    };
                    let input_ports: Vec<_> = node
                        .input_ports
                        .iter()
                        .filter(|port| port.signal_kind == crate::audio::graph::SignalKind::Audio)
                        .collect();
                    let output_ports: Vec<_> = node
                        .output_ports
                        .iter()
                        .filter(|port| port.signal_kind == crate::audio::graph::SignalKind::Audio)
                        .collect();
                    for (input_index, port) in input_ports.iter().enumerate() {
                        self.plugin_inputs[input_index].clear();
                        for reference in self.incoming[idx]
                            .iter()
                            .filter(|reference| reference.to_port == port.id)
                        {
                            let source = if reference.source_index < idx {
                                self.current_ports[reference.source_index]
                                    .get(reference.source_port_index)
                            } else {
                                self.previous_ports[reference.source_index]
                                    .get(reference.source_port_index)
                            };
                            let Some(source) = source else { continue };
                            self.plugin_inputs[input_index].merge_oldest_latency(source);
                            let gain = crate::audio::format::db_to_linear(reference.gain_db);
                            self.delays[reference.delay_index].mix_into(
                                &source.samples,
                                &mut self.plugin_inputs[input_index].samples,
                                gain,
                            );
                            if reference.mute {
                                self.plugin_inputs[input_index].samples.fill(0.0);
                            }
                        }
                    }
                    for output in self.plugin_outputs.iter_mut().take(output_ports.len()) {
                        output.clear();
                    }
                    if *bypass {
                        for (output, input) in self
                            .plugin_outputs
                            .iter_mut()
                            .zip(self.plugin_inputs.iter())
                        {
                            output.samples.copy_from_slice(&input.samples);
                            output.copy_latency_from(input);
                        }
                        Ok(())
                    } else if let Some(processor) = bindings.processors.get(&node_id) {
                        if *failure_policy == TransformFailurePolicy::Bypass {
                            self.backup[idx]
                                .samples
                                .copy_from_slice(&self.plugin_inputs[0].samples);
                        }
                        match processor.process_ports(
                            &mut self.plugin_inputs[..input_ports.len()],
                            &mut self.plugin_outputs[..output_ports.len()],
                        ) {
                            Ok(()) => {
                                for output in
                                    self.plugin_outputs.iter_mut().take(output_ports.len())
                                {
                                    output.apply_gain_db(*gain_db);
                                    if *mute {
                                        output.clear();
                                    }
                                }
                                self.current[idx]
                                    .samples
                                    .copy_from_slice(&self.plugin_outputs[0].samples);
                                self.current[idx].copy_latency_from(&self.plugin_outputs[0]);
                                for (slot, output) in self.current_ports[idx]
                                    .iter_mut()
                                    .zip(self.plugin_outputs.iter())
                                {
                                    slot.samples.copy_from_slice(&output.samples);
                                    slot.copy_latency_from(output);
                                }
                                let sample_rate = self.current[idx].format.sample_rate;
                                if let Some(bank) = bank.as_ref() {
                                    bank.plugin_queue.record_us(frames_to_us(
                                        processor.queued_frames() as usize,
                                        sample_rate,
                                    ));
                                    bank.plugin_algo.record_us(frames_to_us(
                                        processor.latency_frames() as usize,
                                        sample_rate,
                                    ));
                                }
                                self.current[idx]
                                    .shift_capture_time(processor.latency_frames() as usize);
                                Ok(())
                            }
                            Err(err) => {
                                match failure_policy {
                                    TransformFailurePolicy::Bypass => self.current[idx]
                                        .samples
                                        .copy_from_slice(&self.backup[idx].samples),
                                    TransformFailurePolicy::Silence
                                    | TransformFailurePolicy::StopNode => self.current[idx].clear(),
                                }
                                Err(err)
                            }
                        }
                    } else {
                        // Missing plugin/instance never leaks untransformed audio.
                        match failure_policy {
                            TransformFailurePolicy::Bypass => {
                                self.current[idx]
                                    .samples
                                    .copy_from_slice(&self.plugin_inputs[0].samples);
                            }
                            TransformFailurePolicy::Silence | TransformFailurePolicy::StopNode => {
                                self.current[idx].clear()
                            }
                        }
                        Ok(())
                    }
                }
                // Legacy v0.1 RVC path retained only for configuration migration.
                NodeClass::Transform if node.node_type == NodeType::Rvc => {
                    self.copy_first_input(idx);
                    if let NodeProperties::Rvc {
                        bypass,
                        auto_bypass_on_error,
                        ..
                    } = &node.properties
                    {
                        if *bypass {
                            Ok(())
                        } else if let Some(processor) = bindings.processors.get(&node_id) {
                            if *auto_bypass_on_error {
                                self.backup[idx]
                                    .samples
                                    .copy_from_slice(&self.current[idx].samples);
                            }
                            match processor.process_in_place(&mut self.current[idx]) {
                                Ok(()) => Ok(()),
                                Err(err) => {
                                    if *auto_bypass_on_error {
                                        self.current[idx]
                                            .samples
                                            .copy_from_slice(&self.backup[idx].samples);
                                    } else {
                                        self.current[idx].clear();
                                    }
                                    Err(err)
                                }
                            }
                        } else {
                            self.current[idx].clear();
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
                }
                NodeClass::Transform => {
                    self.copy_first_input(idx);
                    Ok(())
                }
                NodeClass::Output => {
                    self.copy_first_input(idx);
                    Ok(())
                }
            };

            if let Some(bank) = bank.as_ref() {
                bank.graph
                    .record_us(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
            }

            if node.node_type.class() == NodeClass::Input && emit_meters {
                // Meter before gain/mute so mute does not hide physical input activity.
                let _ = tx.try_send(RuntimeEvent::NodeMeter {
                    node_id,
                    meter: measure(&self.current[idx].samples),
                });
            }

            if let Some((gain_db, mute)) = node.properties.gain_mute() {
                self.current[idx].apply_gain_db(gain_db);
                if mute {
                    self.current[idx].clear();
                }
            }
            if node.node_type != NodeType::TransformPlugin || self.current_ports[idx].len() <= 1 {
                for output in self.current_ports[idx].iter_mut() {
                    output.samples.copy_from_slice(&self.current[idx].samples);
                    output.copy_latency_from(&self.current[idx]);
                }
            } else if let Some(output) = self.current_ports[idx].first_mut() {
                output.samples.copy_from_slice(&self.current[idx].samples);
                output.copy_latency_from(&self.current[idx]);
            }

            if let Some(tap) = bindings
                .replay_taps
                .get(&node_id)
                .filter(|_| audition.is_none_or(|session| session.target.continue_outputs))
            {
                tap.push_interleaved(&self.current[idx].samples);
            }

            if node.node_type.class() != NodeClass::Input && emit_meters {
                let _ = tx.try_send(RuntimeEvent::NodeMeter {
                    node_id,
                    meter: measure(&self.current[idx].samples),
                });
            }

            if node.node_type.class() == NodeClass::Output {
                if let Some(sink) = bindings
                    .sinks
                    .get(&node_id)
                    .filter(|_| audition.is_none_or(|session| session.target.continue_outputs))
                {
                    if let Err(err) = sink.write(&self.current[idx]) {
                        let _ = tx.try_send(RuntimeEvent::NodeState {
                            node_id,
                            state: RuntimeState::Error,
                            message: Some(err.to_string()),
                        });
                    }
                }
                if let (Some(bank), Some(captured)) =
                    (bank.as_ref(), self.current[idx].capture_time_100ns)
                {
                    let padding_frames = bindings
                        .render_padding
                        .get(&node_id)
                        .map(|padding| padding.load(Ordering::Relaxed) as usize)
                        .unwrap_or(0);
                    let padding_us = frames_to_us(padding_frames, INTERNAL_SAMPLE_RATE);
                    bank.render.record_us(padding_us);
                    bank.e2e.record_us(
                        duration_100ns_to_us(block_now.saturating_sub(captured))
                            .saturating_add(padding_us),
                    );
                    bank.set_accuracy(self.current[idx].accuracy);
                }
            }

            if let Err(err) = result {
                let _ = tx.try_send(RuntimeEvent::NodeState {
                    node_id,
                    state: RuntimeState::Error,
                    message: Some(err.to_string()),
                });
            }
        }
        if let Some(session) = audition {
            // 全ノードの処理済み値から試聴専用へ分岐する。通常配送の継続はモードで決める。
            if let Some(idx) = self
                .order
                .iter()
                .position(|id| *id == session.target.node_id)
            {
                let preview = &mut self.audition_block;
                preview.clear();
                if let Some(port) = session.target.input_port {
                    for input in self.incoming[idx]
                        .iter()
                        .filter(|input| input.to_port == port && !input.mute)
                    {
                        let source = if input.source_index < idx {
                            &self.current_ports[input.source_index]
                        } else {
                            &self.previous_ports[input.source_index]
                        };
                        if let Some(block) = source.get(input.source_port_index) {
                            let gain = 10.0_f32.powf(input.gain_db / 20.0);
                            for (out, value) in preview.samples.iter_mut().zip(&block.samples) {
                                *out += value * gain;
                            }
                        }
                    }
                } else {
                    let port_index = session
                        .target
                        .output_port
                        .and_then(|id| {
                            graph
                                .nodes
                                .get(&session.target.node_id)?
                                .output_ports
                                .iter()
                                .position(|port| port.id == id)
                        })
                        .unwrap_or(0);
                    if let Some(block) = self.current_ports[idx].get(port_index) {
                        preview.samples.copy_from_slice(&block.samples);
                    }
                }
                if let Some(port) = session.target.comparison_input {
                    self.comparison_raw.clear();
                    self.comparison_aligned.clear();
                    for input in self.incoming[idx]
                        .iter()
                        .filter(|input| input.to_port == port && !input.mute)
                    {
                        let source = if input.source_index < idx {
                            &self.current_ports[input.source_index]
                        } else {
                            &self.previous_ports[input.source_index]
                        };
                        if let Some(block) = source.get(input.source_port_index) {
                            let gain = 10.0_f32.powf(input.gain_db / 20.0);
                            for (value, sample) in
                                self.comparison_raw.samples.iter_mut().zip(&block.samples)
                            {
                                *value += sample * gain;
                            }
                        }
                    }
                    if let Some(delay) = self.comparison_delays.get_mut(&session.target.node_id) {
                        delay.mix_into(
                            &self.comparison_raw.samples,
                            &mut self.comparison_aligned.samples,
                            1.0,
                        );
                    } else {
                        self.comparison_aligned
                            .samples
                            .copy_from_slice(&self.comparison_raw.samples);
                    }
                    if session.target.input_port == Some(port) {
                        preview
                            .samples
                            .copy_from_slice(&self.comparison_aligned.samples);
                    }
                    if let Some(mut meter) = session.comparison.try_lock() {
                        meter.observe(
                            graph.revision,
                            &self.comparison_aligned.samples,
                            &self.current[idx].samples,
                        );
                        if session.target.match_rms && meter.report.ready && !meter.report.silent {
                            preview.apply_gain_db(if session.target.input_port.is_some() {
                                meter.report.original_gain_db
                            } else {
                                meter.report.processed_gain_db
                            });
                        }
                    }
                }
                preview.apply_gain_db(session.target.gain_db.min(0.0));
                if let Err(error) = session.sink.write(preview) {
                    session.failed.store(true, Ordering::Release);
                    let _ = tx.try_send(RuntimeEvent::NodeState {
                        node_id: session.target.node_id,
                        state: RuntimeState::Error,
                        message: Some(format!("試聴出力: {error}")),
                    });
                }
            }
        }
        if emit_meters {
            emit_end_to_end_latency(graph, &self.order, &self.latency_banks, tx);
        }
        std::mem::swap(&mut self.current, &mut self.previous);
        std::mem::swap(&mut self.current_ports, &mut self.previous_ports);
    }

    fn copy_first_input(&mut self, target_index: usize) {
        let Some(input) = self.incoming[target_index].first().copied() else {
            return;
        };
        self.copy_source_port(target_index, input.source_index, input.source_port_index);
        if input.mute {
            self.current[target_index].clear();
        } else {
            self.current[target_index].apply_gain_db(input.gain_db);
        }
    }

    fn copy_selected_input(&mut self, target_index: usize, node: &crate::audio::graph::NodeSpec) {
        let NodeProperties::InputSelector { config, .. } = &node.properties else {
            return;
        };
        let Some((state, ports, levels)) = self.switchers.get_mut(&node.id) else {
            return;
        };
        levels.clear();
        for input in &self.incoming[target_index] {
            let source = if input.source_index < target_index {
                &self.current_ports[input.source_index]
            } else {
                &self.previous_ports[input.source_index]
            };
            if let Some(block) = source.get(input.source_port_index) {
                let sum: f32 = block.samples.iter().map(|value| value * value).sum();
                let rms = if input.mute {
                    0.0
                } else {
                    (sum / block.samples.len().max(1) as f32).sqrt()
                        * 10.0_f32.powf(input.gain_db / 20.0)
                };
                levels.push((input.to_port, rms));
            }
        }
        state.advance(config, ports, levels, self.frames, INTERNAL_SAMPLE_RATE);
        for frame in 0..self.frames {
            let selected = state.next_frame();
            if let Some(input) = self.incoming[target_index]
                .iter()
                .find(|input| Some(input.to_port) == selected.port_id && !input.mute)
            {
                let source = if input.source_index < target_index {
                    &self.current_ports[input.source_index]
                } else {
                    &self.previous_ports[input.source_index]
                };
                if let Some(block) = source.get(input.source_port_index) {
                    let gain = selected.gain * 10.0_f32.powf(input.gain_db / 20.0);
                    self.current[target_index].copy_latency_from(block);
                    self.current[target_index].samples[frame * 2] = block.samples[frame * 2] * gain;
                    self.current[target_index].samples[frame * 2 + 1] =
                        block.samples[frame * 2 + 1] * gain;
                }
            }
        }
    }

    fn copy_source_port(
        &mut self,
        target_index: usize,
        source_index: usize,
        source_port_index: usize,
    ) {
        if source_index < target_index {
            let Some(source) = self.current_ports[source_index].get(source_port_index) else {
                return;
            };
            self.current[target_index]
                .samples
                .copy_from_slice(&source.samples);
            self.current[target_index].copy_latency_from(source);
        } else {
            let Some(source) = self.previous_ports[source_index].get(source_port_index) else {
                return;
            };
            self.current[target_index]
                .samples
                .copy_from_slice(&source.samples);
            self.current[target_index].copy_latency_from(source);
        }
    }

    fn mix_multi_input(
        &mut self,
        target_index: usize,
        node: &crate::audio::graph::NodeSpec,
        tx: &Sender<RuntimeEvent>,
        emit_meters: bool,
    ) {
        let slots = match &node.properties {
            NodeProperties::MultiInput { slots, .. } => slots,
            _ => return,
        };
        let count = self.incoming[target_index].len();
        for k in 0..count {
            let incoming = self.incoming[target_index][k];
            let (gain_db, mute) = slots
                .iter()
                .find(|s| s.port_id == incoming.to_port)
                .map(|s| (s.gain_db, s.mute))
                .unwrap_or((incoming.gain_db, incoming.mute));
            let gain = if mute {
                0.0
            } else {
                crate::audio::format::db_to_linear(gain_db)
            };
            if incoming.source_index < target_index {
                let (before, target_and_after) = self.current.split_at_mut(target_index);
                let src = &before[incoming.source_index].samples;
                if emit_meters {
                    let _ = tx.try_send(RuntimeEvent::PortMeter {
                        node_id: node.id,
                        port_id: incoming.to_port,
                        meter: measure(src),
                    });
                }
                self.delays[incoming.delay_index].mix_into(
                    src,
                    &mut target_and_after[0].samples,
                    gain,
                );
            } else {
                let src = &self.previous[incoming.source_index].samples;
                if emit_meters {
                    let _ = tx.try_send(RuntimeEvent::PortMeter {
                        node_id: node.id,
                        port_id: incoming.to_port,
                        meter: measure(src),
                    });
                }
                self.delays[incoming.delay_index].mix_into(
                    src,
                    &mut self.current[target_index].samples,
                    gain,
                );
            }
        }
        let count = self.incoming[target_index].len();
        for k in 0..count {
            let incoming = self.incoming[target_index][k];
            if incoming.source_index < target_index {
                let (before, target_and_after) = self.current.split_at_mut(target_index);
                target_and_after[0].merge_oldest_latency(&before[incoming.source_index]);
            } else {
                let accuracy = self.previous[incoming.source_index].accuracy;
                let time = self.previous[incoming.source_index].capture_time_100ns;
                let mut other = AudioBlock::silence(0);
                other.capture_time_100ns = time;
                other.accuracy = accuracy;
                self.current[target_index].merge_oldest_latency(&other);
            }
        }
    }
}

/// Returns stable manifest keys for process-plugin input connections. Older
/// saved RVC graphs predate `plugin_port_key` and retain random PortIds. RVC
/// has exactly one input, so mapping that one legacy port to `audio_in` is
/// unambiguous. Do not guess for a multi-input legacy graph.
fn connected_process_plugin_ports(
    node: &crate::audio::graph::NodeSpec,
    graph: &GraphSnapshot,
) -> Result<Vec<String>> {
    let is_rvc = matches!(
        &node.properties,
        NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == crate::plugins::RVC_CUDA_PLUGIN_ID
    );
    let has_legacy_rvc_port = is_rvc
        && node
            .input_ports
            .iter()
            .any(|port| port.plugin_port_key.is_none());
    if has_legacy_rvc_port && node.input_ports.len() != 1 {
        return Err(BackendError::InvalidOperation(format!(
            "RVC旧プロジェクトの入力Portをaudio_inへ対応付けできません: 入力Port数={}",
            node.input_ports.len()
        )));
    }
    Ok(node
        .input_ports
        .iter()
        .filter(|port| {
            graph
                .edges
                .iter()
                .any(|edge| edge.to_node == node.id && edge.to_port == port.id)
        })
        .map(|port| {
            port.plugin_port_key.clone().unwrap_or_else(|| {
                if is_rvc {
                    "audio_in".into()
                } else {
                    port.id.to_string()
                }
            })
        })
        .collect())
}

fn emit_end_to_end_latency(
    graph: &GraphSnapshot,
    order: &[NodeId],
    banks: &[Option<Arc<LatencyBank>>],
    tx: &Sender<RuntimeEvent>,
) {
    let mut best: Option<(NodeId, LatencySnapshot, LatencyAccuracy)> = None;
    for (idx, node_id) in order.iter().enumerate() {
        let Some(node) = graph.nodes.get(node_id) else {
            continue;
        };
        if node.node_type.class() != NodeClass::Output {
            continue;
        }
        let Some(bank) = banks.get(idx).and_then(|slot| slot.as_ref()) else {
            continue;
        };
        let snapshot = bank.e2e.snapshot();
        let better = best
            .as_ref()
            .is_none_or(|(_, current, _)| snapshot.ema_ms >= current.ema_ms);
        if better {
            best = Some((*node_id, snapshot, bank.accuracy()));
        }
    }
    if let Some((node_id, snapshot, accuracy)) = best {
        let _ = tx.try_send(RuntimeEvent::LatencyChanged {
            node_id,
            stage: LatencyStage::EndToEnd,
            snapshot,
            accuracy,
        });
    }
}

fn path_latency_reports(graph: &GraphSnapshot, banks: &LatencyMap) -> Vec<PathLatencyReport> {
    let inputs: Vec<NodeId> = graph
        .nodes
        .values()
        .filter(|node| node.node_type.class() == NodeClass::Input)
        .map(|node| node.id)
        .collect();
    let outputs: Vec<NodeId> = graph
        .nodes
        .values()
        .filter(|node| node.node_type.class() == NodeClass::Output)
        .map(|node| node.id)
        .collect();
    let mut reports = Vec::new();
    for input in inputs {
        for output in &outputs {
            if !crate::routing::traversal::path_exists(graph, input, *output) {
                continue;
            }
            let Ok(down) = crate::routing::traversal::downstream_nodes(graph, input) else {
                continue;
            };
            let Ok(up) = crate::routing::traversal::upstream_nodes(graph, *output) else {
                continue;
            };
            let mut on_path: Vec<NodeId> = down
                .into_iter()
                .filter(|node_id| *node_id == *output || up.contains(node_id))
                .collect();
            on_path.push(input);
            if !on_path.contains(output) {
                on_path.push(*output);
            }
            on_path.sort();
            on_path.dedup();
            let mut stages = Vec::new();
            let mut accuracy = LatencyAccuracy::DeviceReported;
            let mut saw_stamp = false;
            for node_id in &on_path {
                let Some(node) = graph.nodes.get(node_id) else {
                    continue;
                };
                let Some(bank) = banks.get(node_id) else {
                    continue;
                };
                match node.node_type.class() {
                    NodeClass::Input => {
                        stages.push(PathLatencyStage {
                            node_id: *node_id,
                            stage: LatencyStage::Capture,
                            snapshot: bank.capture.snapshot(),
                        });
                        accuracy = accuracy.merge(bank.accuracy());
                        saw_stamp = true;
                    }
                    NodeClass::Transform => {
                        stages.push(PathLatencyStage {
                            node_id: *node_id,
                            stage: LatencyStage::Graph,
                            snapshot: bank.graph.snapshot(),
                        });
                        if node.node_type == NodeType::TransformPlugin
                            || node.node_type == NodeType::Rvc
                        {
                            let plugin = LatencySnapshot::from_us(
                                bank.plugin_combined_us(),
                                bank.plugin_combined_us(),
                                bank.plugin_queue
                                    .snapshot()
                                    .max_ms
                                    .mul_add(1000.0, bank.plugin_algo.snapshot().max_ms * 1000.0)
                                    .clamp(0.0, u32::MAX as f64)
                                    as u32,
                            );
                            stages.push(PathLatencyStage {
                                node_id: *node_id,
                                stage: LatencyStage::Plugin,
                                snapshot: plugin,
                            });
                        }
                    }
                    NodeClass::Output => {
                        stages.push(PathLatencyStage {
                            node_id: *node_id,
                            stage: LatencyStage::Render,
                            snapshot: bank.render.snapshot(),
                        });
                        accuracy = accuracy.merge(bank.accuracy());
                        saw_stamp = true;
                    }
                }
            }
            let e2e = banks
                .get(output)
                .map(|bank| bank.e2e.snapshot())
                .unwrap_or_default();
            if !saw_stamp {
                accuracy = LatencyAccuracy::HostEstimated;
            }
            reports.push(PathLatencyReport {
                input_node: input,
                output_node: *output,
                e2e,
                stages,
                accuracy,
            });
        }
    }
    reports
}
