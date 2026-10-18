use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    audio::{
        buffer::AudioRingBuffer,
        engine::AudioSource,
        format::{AudioBlock, DEFAULT_BLOCK_FRAMES, INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
        graph::CaptureMethod,
        meter::{measure, AtomicMeter, MeterFrame},
    },
    error::{BackendError, Result},
    replay::player::ReplaySource,
    replay::state::ReplayStateSnapshot,
    windows::{
        wasapi_capture::{
            start_endpoint_loopback, start_microphone_capture_with_method, CaptureHandle,
        },
        wasapi_render::{start_render, RenderHandle},
        worker_status::{DeviceWorkerSnapshot, DeviceWorkerStatus},
    },
};

fn monitor_ring(block_frames: usize) -> AudioRingBuffer {
    // Four graph blocks absorb ordinary scheduling jitter. Bound the result
    // between 20 and 200 ms so the selected latency preset influences monitor
    // responsiveness without allowing seconds of stale audio to accumulate.
    let capacity_frames = block_frames.saturating_mul(4).clamp(
        INTERNAL_SAMPLE_RATE as usize / 50,
        INTERNAL_SAMPLE_RATE as usize / 5,
    );
    let sample_capacity = capacity_frames * INTERNAL_CHANNELS as usize;
    AudioRingBuffer::new_overwriting(sample_capacity)
}

pub trait ReplayPreviewSource: AudioSource + ReplaySource {}

impl<T> ReplayPreviewSource for T where T: AudioSource + ReplaySource {}

pub enum PreviewSource {
    Replay(std::sync::Arc<dyn ReplayPreviewSource>),
    Audio(Box<dyn AudioSource>),
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InputMonitorSnapshot {
    pub registered_node_id: crate::audio::graph::RegisteredNodeId,
    #[serde(default)]
    pub scope: InputMonitorScope,
    #[serde(default)]
    pub generation: u64,
    pub input_endpoint_id: String,
    pub output_endpoint_id: String,
    pub capture: DeviceWorkerSnapshot,
    pub render: DeviceWorkerSnapshot,
    #[serde(default)]
    pub input_meter: Option<MeterFrame>,
    #[serde(default)]
    pub replay: Option<ReplayStateSnapshot>,
}

#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum InputMonitorScope {
    NodeManagement,
    Routing,
    Playback,
    #[default]
    Legacy,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InputMonitorStatesSnapshot {
    pub generation: u64,
    pub sessions: Vec<InputMonitorSnapshot>,
}

pub struct InputMonitorManager {
    state: parking_lot::Mutex<InputMonitorManagerState>,
}

struct InputMonitorManagerState {
    sessions: HashMap<crate::audio::graph::RegisteredNodeId, InputMonitorSession>,
    starting: HashMap<crate::audio::graph::RegisteredNodeId, PendingMonitorStart>,
    generation: u64,
    next_operation: u64,
    last_started: Option<crate::audio::graph::RegisteredNodeId>,
}

#[derive(Clone, Copy)]
struct PendingMonitorStart {
    scope: InputMonitorScope,
    token: u64,
}

impl Default for InputMonitorManager {
    fn default() -> Self {
        Self {
            state: parking_lot::Mutex::new(InputMonitorManagerState {
                sessions: HashMap::new(),
                starting: HashMap::new(),
                generation: 0,
                next_operation: 0,
                last_started: None,
            }),
        }
    }
}

impl InputMonitorManager {
    pub fn begin_start(
        &self,
        registered_node_id: crate::audio::graph::RegisteredNodeId,
        scope: InputMonitorScope,
    ) -> Result<(Option<InputMonitorSession>, u64)> {
        let mut state = self.state.lock();
        if state.starting.contains_key(&registered_node_id) {
            return Err(BackendError::InvalidOperation(
                "このノードのモニター操作は既に進行中です".into(),
            ));
        }
        state.next_operation = state.next_operation.saturating_add(1);
        let token = state.next_operation;
        state
            .starting
            .insert(registered_node_id, PendingMonitorStart { scope, token });
        let previous = state.sessions.remove(&registered_node_id);
        if previous.is_some() {
            state.generation = state.generation.saturating_add(1);
        }
        Ok((previous, token))
    }

    pub fn finish_start(
        &self,
        registered_node_id: crate::audio::graph::RegisteredNodeId,
        token: u64,
        session: InputMonitorSession,
    ) -> Result<InputMonitorSnapshot> {
        let mut state = self.state.lock();
        if state
            .starting
            .get(&registered_node_id)
            .is_none_or(|pending| pending.token != token)
        {
            return Err(BackendError::InvalidOperation(
                "モニター開始は停止操作によって取り消されました".into(),
            ));
        }
        state.starting.remove(&registered_node_id);
        state.sessions.insert(registered_node_id, session);
        state.last_started = Some(registered_node_id);
        state.generation = state.generation.saturating_add(1);
        Ok(snapshot_for(&state, registered_node_id)
            .expect("a session inserted by finish_start must be present"))
    }

    pub fn cancel_start(&self, registered_node_id: crate::audio::graph::RegisteredNodeId) {
        self.state.lock().starting.remove(&registered_node_id);
    }

    pub fn stop_for(
        &self,
        registered_node_id: crate::audio::graph::RegisteredNodeId,
    ) -> InputMonitorStatesSnapshot {
        let removed = {
            let mut state = self.state.lock();
            state.starting.remove(&registered_node_id);
            let removed = state.sessions.remove(&registered_node_id);
            if removed.is_some() {
                state.generation = state.generation.saturating_add(1);
            }
            if state.last_started == Some(registered_node_id) {
                state.last_started = None;
            }
            removed
        };
        drop(removed);
        self.states()
    }

    pub fn stop_scope(&self, scope: InputMonitorScope) -> InputMonitorStatesSnapshot {
        let removed = {
            let mut state = self.state.lock();
            let ids = state
                .sessions
                .iter()
                .filter_map(|(id, session)| (session.scope == scope).then_some(*id))
                .collect::<Vec<_>>();
            let removed = ids
                .iter()
                .filter_map(|id| state.sessions.remove(id))
                .collect::<Vec<_>>();
            if !removed.is_empty() {
                state.generation = state.generation.saturating_add(1);
            }
            state.starting.retain(|_, pending| pending.scope != scope);
            if state
                .last_started
                .is_some_and(|id| !state.sessions.contains_key(&id))
            {
                state.last_started = None;
            }
            removed
        };
        drop(removed);
        self.states()
    }

    pub fn stop_all(&self) {
        let removed = {
            let mut state = self.state.lock();
            let removed = std::mem::take(&mut state.sessions);
            state.starting.clear();
            state.last_started = None;
            if !removed.is_empty() {
                state.generation = state.generation.saturating_add(1);
            }
            removed
        };
        drop(removed);
    }

    pub fn states(&self) -> InputMonitorStatesSnapshot {
        let state = self.state.lock();
        let mut sessions = state
            .sessions
            .keys()
            .filter_map(|id| snapshot_for(&state, *id))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|snapshot| snapshot.registered_node_id);
        InputMonitorStatesSnapshot {
            generation: state.generation,
            sessions,
        }
    }

    pub fn legacy_state(&self) -> Option<InputMonitorSnapshot> {
        let state = self.state.lock();
        state
            .last_started
            .and_then(|id| snapshot_for(&state, id))
            .or_else(|| {
                state
                    .sessions
                    .keys()
                    .min()
                    .and_then(|id| snapshot_for(&state, *id))
            })
    }

    pub fn with_session<T>(
        &self,
        registered_node_id: crate::audio::graph::RegisteredNodeId,
        operation: impl FnOnce(&InputMonitorSession) -> Result<T>,
    ) -> Result<(T, InputMonitorSnapshot)> {
        let mut state = self.state.lock();
        let result = {
            let session = state.sessions.get(&registered_node_id).ok_or_else(|| {
                BackendError::InvalidOperation("指定された入力モニターが開始されていません".into())
            })?;
            operation(session)?
        };
        state.generation = state.generation.saturating_add(1);
        let snapshot = snapshot_for(&state, registered_node_id)
            .expect("the borrowed session must still be present");
        Ok((result, snapshot))
    }
}

fn snapshot_for(
    state: &InputMonitorManagerState,
    registered_node_id: crate::audio::graph::RegisteredNodeId,
) -> Option<InputMonitorSnapshot> {
    let mut snapshot = state.sessions.get(&registered_node_id)?.snapshot();
    snapshot.generation = state.generation;
    Some(snapshot)
}

pub struct InputMonitorSession {
    registered_node_id: crate::audio::graph::RegisteredNodeId,
    scope: InputMonitorScope,
    input_endpoint_id: String,
    output_endpoint_id: String,
    capture: Option<CaptureHandle>,
    preview: Option<PreviewHandle>,
    render: RenderHandle,
    ring: AudioRingBuffer,
}

struct PreviewHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    status: DeviceWorkerStatus,
    meter: std::sync::Arc<AtomicMeter>,
    replay: std::sync::Arc<parking_lot::Mutex<Option<ReplayStateSnapshot>>>,
    replay_source: Option<std::sync::Arc<dyn ReplayPreviewSource>>,
}

impl PreviewHandle {
    fn start(source: PreviewSource, ring: AudioRingBuffer) -> Result<Self> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (audio_source, replay_source): (
            std::sync::Arc<dyn AudioSource>,
            Option<std::sync::Arc<dyn ReplayPreviewSource>>,
        ) = match source {
            PreviewSource::Replay(source) => (source.clone(), Some(source)),
            PreviewSource::Audio(source) => (std::sync::Arc::from(source), None),
        };
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let status = DeviceWorkerStatus::capture("preview");
        let worker_status = status.clone();
        let meter = std::sync::Arc::new(AtomicMeter::default());
        let worker_meter = meter.clone();
        let replay = std::sync::Arc::new(parking_lot::Mutex::new(audio_source.replay_snapshot()));
        let worker_replay = replay.clone();
        let worker_source = audio_source.clone();
        let join = std::thread::Builder::new()
            .name("input-preview-source".into())
            .spawn(move || {
                worker_status.running_with_method("preview");
                let mut block = AudioBlock::silence(DEFAULT_BLOCK_FRAMES);
                let block_duration = Duration::from_secs_f64(
                    DEFAULT_BLOCK_FRAMES as f64 / INTERNAL_SAMPLE_RATE as f64,
                );
                let mut next_deadline = Instant::now();
                while !worker_stop.load(Ordering::Acquire) {
                    block.clear();
                    if worker_source.read_into(&mut block).is_err() {
                        worker_status.reconnecting("プレビュー音声の読み取りに失敗しました");
                        break;
                    }
                    worker_meter.store(measure(&block.samples));
                    *worker_replay.lock() = worker_source.replay_snapshot();
                    let _ = ring.push_slice(&block.samples);
                    next_deadline += block_duration;
                    let now = Instant::now();
                    if next_deadline > now {
                        std::thread::sleep(next_deadline - now);
                    } else if now.duration_since(next_deadline) > block_duration {
                        next_deadline = now;
                    }
                }
                worker_status.stopped();
            })
            .map_err(BackendError::Io)?;
        Ok(Self {
            stop,
            join: Some(join),
            status,
            meter,
            replay,
            replay_source,
        })
    }

    fn status(&self) -> DeviceWorkerSnapshot {
        self.status.snapshot()
    }

    fn wait_until_running(&self, timeout: Duration) -> Result<DeviceWorkerSnapshot> {
        self.status.wait_until_running(timeout)
    }

    fn meter(&self) -> MeterFrame {
        self.meter.load()
    }

    fn replay_snapshot(&self) -> Option<ReplayStateSnapshot> {
        self.replay.lock().clone()
    }

    fn replay_source(&self) -> Result<&dyn ReplayPreviewSource> {
        self.replay_source.as_deref().ok_or_else(|| {
            BackendError::InvalidOperation("このプレビューは再生操作に対応していません".into())
        })
    }

    fn play(&self) -> Result<()> {
        self.replay_source()?.play()
    }

    fn pause(&self) -> Result<()> {
        self.replay_source()?.pause()
    }

    fn seek(&self, position: Duration) -> Result<()> {
        self.replay_source()?.seek(position)
    }
}

impl Drop for PreviewHandle {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl InputMonitorSession {
    pub fn start_microphone(
        registered_node_id: crate::audio::graph::RegisteredNodeId,
        scope: InputMonitorScope,
        input_endpoint_id: String,
        capture_method: CaptureMethod,
        output_endpoint_id: String,
        block_frames: usize,
    ) -> Result<Self> {
        let ring = monitor_ring(block_frames);
        // Monitor sessions share one physical output concurrently. Keep these
        // utility streams in shared mode; exclusive render would let the first
        // monitored node lock every other session out of the device.
        let render = start_render(output_endpoint_id.clone(), ring.clone())?;
        let capture = start_microphone_capture_with_method(
            input_endpoint_id.clone(),
            capture_method,
            ring.clone(),
        )?;
        capture.wait_until_running(Duration::from_secs(3))?;
        render.wait_until_running(Duration::from_secs(3))?;
        Ok(Self {
            registered_node_id,
            scope,
            input_endpoint_id,
            output_endpoint_id,
            capture: Some(capture),
            preview: None,
            render,
            ring,
        })
    }

    pub fn start_loopback(
        registered_node_id: crate::audio::graph::RegisteredNodeId,
        scope: InputMonitorScope,
        input_endpoint_id: String,
        output_endpoint_id: String,
        block_frames: usize,
    ) -> Result<Self> {
        let ring = monitor_ring(block_frames);
        let render = start_render(output_endpoint_id.clone(), ring.clone())?;
        let capture = start_endpoint_loopback(input_endpoint_id.clone(), ring.clone())?;
        capture.wait_until_running(Duration::from_secs(3))?;
        render.wait_until_running(Duration::from_secs(3))?;
        Ok(Self {
            registered_node_id,
            scope,
            input_endpoint_id,
            output_endpoint_id,
            capture: Some(capture),
            preview: None,
            render,
            ring,
        })
    }

    pub fn start_preview(
        registered_node_id: crate::audio::graph::RegisteredNodeId,
        scope: InputMonitorScope,
        input_label: String,
        source: PreviewSource,
        output_endpoint_id: String,
        block_frames: usize,
    ) -> Result<Self> {
        let ring = monitor_ring(block_frames);
        let render = start_render(output_endpoint_id.clone(), ring.clone())?;
        let preview = PreviewHandle::start(source, ring.clone())?;
        preview.wait_until_running(Duration::from_secs(3))?;
        render.wait_until_running(Duration::from_secs(3))?;
        Ok(Self {
            registered_node_id,
            scope,
            input_endpoint_id: input_label,
            output_endpoint_id,
            capture: None,
            preview: Some(preview),
            render,
            ring,
        })
    }

    pub fn is_replay_preview_for(
        &self,
        registered_node_id: crate::audio::graph::RegisteredNodeId,
    ) -> bool {
        self.registered_node_id == registered_node_id && self.preview.is_some()
    }

    pub fn play_preview(&self) -> Result<()> {
        self.ring.clear();
        self.preview
            .as_ref()
            .ok_or_else(|| {
                BackendError::InvalidOperation("再生プレビューが開始されていません".into())
            })?
            .play()
    }

    pub fn pause_preview(&self) -> Result<()> {
        let result = self
            .preview
            .as_ref()
            .ok_or_else(|| {
                BackendError::InvalidOperation("再生プレビューが開始されていません".into())
            })?
            .pause();
        self.ring.clear();
        result
    }

    pub fn seek_preview(&self, position: Duration) -> Result<()> {
        let result = self
            .preview
            .as_ref()
            .ok_or_else(|| {
                BackendError::InvalidOperation("再生プレビューが開始されていません".into())
            })?
            .seek(position);
        self.ring.clear();
        result
    }

    pub fn snapshot(&self) -> InputMonitorSnapshot {
        InputMonitorSnapshot {
            registered_node_id: self.registered_node_id,
            scope: self.scope,
            generation: 0,
            input_endpoint_id: self.input_endpoint_id.clone(),
            output_endpoint_id: self.output_endpoint_id.clone(),
            capture: self
                .capture
                .as_ref()
                .map(CaptureHandle::status)
                .or_else(|| self.preview.as_ref().map(PreviewHandle::status))
                .expect("input monitor session must have a source"),
            render: self.render.status(),
            input_meter: self.preview.as_ref().map(PreviewHandle::meter),
            replay: self
                .preview
                .as_ref()
                .and_then(PreviewHandle::replay_snapshot),
        }
    }
}

pub fn unsupported_input_monitor_type() -> BackendError {
    BackendError::InvalidOperation(
        "モニターできるのはマイク入力または再生デバイスCaptureです".into(),
    )
}

pub fn ensure_distinct_monitor_endpoints(
    input_endpoint_id: &str,
    output_endpoint_id: &str,
) -> Result<()> {
    if input_endpoint_id == output_endpoint_id {
        return Err(BackendError::InvalidOperation(
            "モニタリング出力に使用中の再生デバイスは自己ループを防ぐため監視できません".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_distinct_monitor_endpoints, monitor_ring, InputMonitorManager, InputMonitorScope,
    };
    use crate::audio::graph::RegisteredNodeId;

    #[test]
    fn pending_start_is_exclusive_per_node_and_can_be_cancelled() {
        let manager = InputMonitorManager::default();
        let id = RegisteredNodeId::new();
        let (_, first_token) = manager
            .begin_start(id, InputMonitorScope::NodeManagement)
            .unwrap();
        assert!(manager.begin_start(id, InputMonitorScope::Routing).is_err());
        manager.cancel_start(id);
        let (_, second_token) = manager.begin_start(id, InputMonitorScope::Routing).unwrap();
        assert!(second_token > first_token);
    }

    #[test]
    fn stopping_a_scope_cancels_only_matching_pending_starts() {
        let manager = InputMonitorManager::default();
        let management_id = RegisteredNodeId::new();
        let routing_id = RegisteredNodeId::new();
        manager
            .begin_start(management_id, InputMonitorScope::NodeManagement)
            .unwrap();
        manager
            .begin_start(routing_id, InputMonitorScope::Routing)
            .unwrap();

        manager.stop_scope(InputMonitorScope::NodeManagement);
        assert!(manager
            .begin_start(management_id, InputMonitorScope::NodeManagement)
            .is_ok());
        assert!(manager
            .begin_start(routing_id, InputMonitorScope::Routing)
            .is_err());
    }

    #[test]
    fn endpoint_self_monitoring_is_rejected_before_audio_starts() {
        assert!(ensure_distinct_monitor_endpoints("render-a", "render-a").is_err());
        assert!(ensure_distinct_monitor_endpoints("render-a", "render-b").is_ok());
    }

    #[test]
    fn monitor_buffer_follows_block_size_with_safe_latency_bounds() {
        assert_eq!(monitor_ring(128).capacity_samples(), 1_920);
        assert_eq!(monitor_ring(480).capacity_samples(), 3_840);
        assert_eq!(monitor_ring(960).capacity_samples(), 7_680);
        assert_eq!(monitor_ring(usize::MAX).capacity_samples(), 19_200);
    }
}
