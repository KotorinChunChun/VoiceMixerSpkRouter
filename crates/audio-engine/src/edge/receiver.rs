use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::audio::{
    buffer::{JitterBuffer, JitterPolicy},
    format::{INTERNAL_CHANNELS, INTERNAL_SAMPLE_RATE},
    graph::NodeId,
};
use crate::edge::control::{EdgeCaptureState, EdgeControlMessage, EdgeMediaAction, EdgeMediaState};
use crate::edge::stream_resampler::StreamResampler;
use crate::error::{BackendError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStreamState {
    Waiting,
    Running,
    Stopped,
    Timeout,
    Disconnected,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EdgeTabSummary {
    pub stream_key: String,
    pub tab_id: Option<u64>,
    pub title: String,
    pub url: String,
    pub state: EdgeStreamState,
    pub media_state: EdgeMediaState,
    pub media_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EdgeMediaResult {
    pub stream_key: String,
    pub request_id: String,
    pub state: EdgeMediaState,
    pub error: Option<String>,
}

pub struct EdgeStream {
    pub jitter: JitterBuffer,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    connected: AtomicBool,
    capture_stopped: AtomicBool,
    last_heartbeat: Mutex<Instant>,
    resampler: Mutex<StreamResampler>,
    correction: Mutex<EdgeCorrection>,
    tab_id: AtomicU64,
    title: RwLock<String>,
    url: RwLock<String>,
    media_state: RwLock<(EdgeMediaState, Option<String>)>,
}
#[derive(Default)]
struct EdgeCorrection {
    side: i8,
    frames: u64,
}

impl EdgeStream {
    fn new(sample_rate: u32, channels: u16, connected: bool) -> Self {
        Self {
            jitter: JitterBuffer::new(
                INTERNAL_SAMPLE_RATE as usize * 2 * 2,
                JitterPolicy::default(),
            ),
            sample_rate: AtomicU32::new(sample_rate),
            channels: AtomicU32::new(channels as u32),
            connected: AtomicBool::new(connected),
            capture_stopped: AtomicBool::new(false),
            last_heartbeat: Mutex::new(Instant::now()),
            resampler: Mutex::new(
                StreamResampler::new(INTERNAL_CHANNELS).expect("stereo resampler"),
            ),
            correction: Mutex::new(EdgeCorrection::default()),
            tab_id: AtomicU64::new(0),
            title: RwLock::new(String::new()),
            url: RwLock::new(String::new()),
            media_state: RwLock::new((EdgeMediaState::Unknown, None)),
        }
    }
    fn connect(&self, sample_rate: u32, channels: u16) {
        // A reconnect must not replay packets owned by the previous PCM lease.
        self.jitter.reset();
        self.capture_stopped.store(false, Ordering::Release);
        self.sample_rate.store(sample_rate, Ordering::Release);
        self.channels.store(channels as u32, Ordering::Release);
        self.resampler.lock().reset();
        *self.correction.lock() = EdgeCorrection::default();
        self.heartbeat();
    }
    fn set_capture_state(&self, state: EdgeCaptureState) {
        match state {
            EdgeCaptureState::Stopped => {
                self.capture_stopped.store(true, Ordering::Release);
                self.jitter.reset();
                self.resampler.lock().reset();
                self.reset_correction();
            }
            EdgeCaptureState::Running => {
                // Resuming begins a fresh PCM epoch.  Heartbeats deliberately
                // do not change this flag, so a paused extension stays paused.
                self.capture_stopped.store(false, Ordering::Release);
                self.jitter.reset();
                self.resampler.lock().reset();
                self.reset_correction();
                self.heartbeat();
            }
        }
    }
    fn capture_state(&self) -> EdgeCaptureState {
        if self.capture_stopped.load(Ordering::Acquire) {
            EdgeCaptureState::Stopped
        } else {
            EdgeCaptureState::Running
        }
    }
    fn reset_correction(&self) {
        *self.correction.lock() = EdgeCorrection::default();
    }
    fn correction_ratio(&self, input_frames: usize, input_rate: u32) -> f64 {
        if !self.jitter.is_started() {
            self.reset_correction();
            return 1.0;
        }
        let center = (self.jitter.target_fill_ratio() * 0.5).clamp(0.05, 0.9);
        let delta = self.jitter.fill_ratio() - center;
        let side = if delta > 0.025 {
            1
        } else if delta < -0.025 {
            -1
        } else {
            0
        };
        let mut state = self.correction.lock();
        if side == 0 || (state.side != 0 && state.side != side) {
            *state = EdgeCorrection { side, frames: 0 };
            return 1.0;
        }
        state.side = side;
        state.frames += input_frames as u64;
        if state.frames < (input_rate as u64 + 3) / 4 {
            return 1.0;
        }
        let excess = (delta.abs() - 0.025).max(0.0) * if delta > 0.0 { 1.0 } else { -1.0 };
        (1.0 + (-excess * 0.02).clamp(-0.005, 0.005)) as f64
    }
    pub fn update_tab(&self, tab_id: Option<u64>, title: Option<&str>, url: Option<&str>) {
        if let Some(tab_id) = tab_id {
            self.tab_id.store(tab_id, Ordering::Release);
        }
        if let Some(title) = title {
            *self.title.write() = title.to_string();
        }
        if let Some(url) = url {
            *self.url.write() = url.to_string();
        }
    }
    pub fn tab_id(&self) -> Option<u64> {
        let id = self.tab_id.load(Ordering::Acquire);
        (id != 0).then_some(id)
    }
    pub fn title(&self) -> String {
        self.title.read().clone()
    }
    pub fn url(&self) -> String {
        self.url.read().clone()
    }
    fn set_media_state(&self, state: EdgeMediaState, error: Option<String>) {
        *self.media_state.write() = (state, error);
    }
    fn media_state(&self) -> (EdgeMediaState, Option<String>) {
        self.media_state.read().clone()
    }
    pub fn heartbeat(&self) {
        *self.last_heartbeat.lock() = Instant::now();
        self.connected.store(true, Ordering::Release);
    }
    pub fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
    }
    pub fn state(&self, timeout: Duration) -> EdgeStreamState {
        if !self.connected.load(Ordering::Acquire) {
            return EdgeStreamState::Disconnected;
        }
        if self.last_heartbeat.lock().elapsed() > timeout {
            EdgeStreamState::Timeout
        } else if self.capture_stopped.load(Ordering::Acquire) {
            EdgeStreamState::Stopped
        } else {
            EdgeStreamState::Running
        }
    }
}

#[derive(Clone)]
pub struct EdgeHub {
    streams: Arc<RwLock<HashMap<String, Arc<EdgeStream>>>>,
    owners: Arc<Mutex<HashMap<String, u64>>>,
    next_owner: Arc<AtomicU64>,
    jitter_target_fill_milli: Arc<std::sync::atomic::AtomicU32>,
    auth_token: Arc<RwLock<String>>,
    receiver_nodes: Arc<Mutex<HashMap<String, std::collections::HashSet<NodeId>>>>,
    media_connections: Arc<Mutex<HashMap<String, EdgeMediaConnection>>>,
    media_pending: Arc<Mutex<HashMap<(String, String), EdgeMediaPending>>>,
}

#[derive(Clone)]
struct EdgeMediaConnection {
    owner: u64,
    sender: tokio::sync::mpsc::UnboundedSender<EdgeControlMessage>,
}

struct EdgeMediaPending {
    owner: u64,
    sender: tokio::sync::oneshot::Sender<std::result::Result<EdgeMediaResult, String>>,
}

struct EdgeMediaPendingGuard {
    hub: EdgeHub,
    key: String,
    request_id: String,
    owner: u64,
    armed: bool,
}

impl EdgeMediaPendingGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EdgeMediaPendingGuard {
    fn drop(&mut self) {
        if self.armed {
            self.hub
                .remove_media_pending(&self.key, &self.request_id, self.owner);
        }
    }
}

impl Default for EdgeHub {
    fn default() -> Self {
        Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
            owners: Arc::new(Mutex::new(HashMap::new())),
            next_owner: Arc::new(AtomicU64::new(1)),
            jitter_target_fill_milli: Arc::new(std::sync::atomic::AtomicU32::new(500)),
            auth_token: Arc::new(RwLock::new(Uuid::new_v4().to_string())),
            receiver_nodes: Arc::new(Mutex::new(HashMap::new())),
            media_connections: Arc::new(Mutex::new(HashMap::new())),
            media_pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl EdgeHub {
    pub fn auth_token(&self) -> String {
        self.auth_token.read().clone()
    }

    pub fn set_auth_token(&self, token: impl Into<String>) {
        let token = token.into();
        if !token.trim().is_empty() {
            *self.auth_token.write() = token;
        }
    }

    pub fn regenerate_auth_token(&self) -> String {
        let token = Uuid::new_v4().to_string();
        *self.auth_token.write() = token.clone();
        token
    }

    pub fn authenticate(
        &self,
        token: Option<&str>,
        protocol_version: u16,
    ) -> std::result::Result<(), &'static str> {
        if protocol_version != 1 {
            return Err("version");
        }
        if token != Some(self.auth_token.read().as_str()) {
            return Err("bad_token");
        }
        Ok(())
    }
}

impl EdgeHub {
    pub fn register(
        &self,
        key: impl Into<String>,
        sample_rate: u32,
        channels: u16,
    ) -> Arc<EdgeStream> {
        let key = key.into();
        let mut streams = self.streams.write();
        match streams.entry(key) {
            Entry::Occupied(entry) => {
                entry.get().connect(sample_rate, channels);
                entry.get().clone()
            }
            Entry::Vacant(entry) => {
                let stream = Arc::new(EdgeStream::new(sample_rate, channels, true));
                stream
                    .jitter
                    .set_target_fill_ratio(self.jitter_target_fill());
                entry.insert(stream.clone());
                stream
            }
        }
    }

    /// Creates a stable disconnected stream object for a graph node before the
    /// browser extension connects. `register` later reuses this object, so the
    /// AudioSource binding never points at a stale jitter buffer.
    pub fn ensure(
        &self,
        key: impl Into<String>,
        sample_rate: u32,
        channels: u16,
    ) -> Arc<EdgeStream> {
        let key = key.into();
        let mut streams = self.streams.write();
        match streams.entry(key) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let stream = Arc::new(EdgeStream::new(sample_rate, channels, false));
                stream
                    .jitter
                    .set_target_fill_ratio(self.jitter_target_fill());
                entry.insert(stream.clone());
                stream
            }
        }
    }
    pub fn acquire_pcm_lease(
        &self,
        key: impl Into<String>,
        sample_rate: u32,
        channels: u16,
    ) -> std::result::Result<EdgePcmLease, &'static str> {
        let key = key.into();
        let owner = self.next_owner.fetch_add(1, Ordering::Relaxed);
        {
            let mut owners = self.owners.lock();
            if owners.contains_key(&key) {
                return Err("stream_in_use");
            }
            owners.insert(key.clone(), owner);
        }
        self.register(key.clone(), sample_rate, channels);
        Ok(EdgePcmLease {
            hub: self.clone(),
            key,
            owner,
        })
    }
    fn release_pcm_lease(&self, key: &str, owner: u64) {
        // Keep owner comparison, removal, and disconnect in one critical
        // section. acquire_pcm_lease takes owners before streams too, so a
        // stale connection cannot disconnect a newly acquired lease.
        let mut owners = self.owners.lock();
        if owners.get(key).copied() != Some(owner) {
            return;
        }
        owners.remove(key);
        self.remove_media_connection(key, owner, "Edge PCM接続が切断されました");
        if let Some(stream) = self.streams.read().get(key).cloned() {
            stream.disconnect();
        }
    }
    pub fn set_jitter_target_fill(&self, fill: f32) {
        let milli = (fill.clamp(0.05, 0.95) * 1000.0).round() as u32;
        self.jitter_target_fill_milli
            .store(milli, std::sync::atomic::Ordering::Release);
        for stream in self.streams.read().values() {
            stream.jitter.set_target_fill_ratio(fill);
            stream.reset_correction();
        }
    }

    fn jitter_target_fill(&self) -> f32 {
        self.jitter_target_fill_milli
            .load(std::sync::atomic::Ordering::Acquire) as f32
            / 1000.0
    }

    pub fn get(&self, key: &str) -> Option<Arc<EdgeStream>> {
        self.streams.read().get(key).cloned()
    }

    pub fn list_tabs(&self) -> Vec<EdgeTabSummary> {
        let mut tabs: Vec<_> = self
            .streams
            .read()
            .iter()
            .map(|(stream_key, stream)| {
                let (media_state, media_error) = stream.media_state();
                EdgeTabSummary {
                    stream_key: stream_key.clone(),
                    tab_id: stream.tab_id(),
                    title: stream.title(),
                    url: stream.url(),
                    state: stream.state(Duration::from_secs(3)),
                    media_state,
                    media_error,
                }
            })
            .collect();
        tabs.sort_by(|a, b| a.stream_key.cmp(&b.stream_key));
        tabs
    }
    pub fn remove(&self, key: &str) {
        // Keep the stable stream object in the hub. Active graph sources hold
        // its jitter buffer, so replacing the object on a later reconnect
        // would send PCM to a buffer the graph no longer reads.
        if let Some(s) = self.streams.read().get(key).cloned() {
            s.disconnect();
        }
    }
    pub fn heartbeat(&self, key: &str) -> Result<()> {
        let stream = self.get(key).ok_or_else(|| {
            BackendError::InvalidOperation(format!("unknown Edge stream key: {key}"))
        })?;
        stream.heartbeat();
        Ok(())
    }

    pub fn set_capture_state(&self, key: &str, state: EdgeCaptureState) -> Result<()> {
        let stream = self.get(key).ok_or_else(|| {
            BackendError::InvalidOperation(format!("unknown Edge stream key: {key}"))
        })?;
        stream.set_capture_state(state);
        Ok(())
    }

    pub fn capture_state(&self, key: &str) -> EdgeCaptureState {
        self.get(key)
            .map(|stream| stream.capture_state())
            .unwrap_or(EdgeCaptureState::Stopped)
    }

    pub fn set_receiver_enabled(&self, key: &str, node_id: NodeId, enabled: bool) {
        let mut receivers = self.receiver_nodes.lock();
        let nodes = receivers.entry(key.to_string()).or_default();
        if enabled {
            nodes.insert(node_id);
        } else {
            nodes.remove(&node_id);
            if nodes.is_empty() {
                receivers.remove(key);
            }
        }
    }

    /// Removes a graph node from every stream.  Runtime teardown can happen
    /// after the controller has already deleted its NodeSpec, so relying on
    /// the former stream key would otherwise leave a stale receiver alive.
    pub fn remove_receiver_node(&self, node_id: NodeId) {
        let mut receivers = self.receiver_nodes.lock();
        receivers.retain(|_, nodes| {
            nodes.remove(&node_id);
            !nodes.is_empty()
        });
    }

    pub fn receiver_state(&self, key: &str) -> EdgeCaptureState {
        if self
            .receiver_nodes
            .lock()
            .get(key)
            .is_some_and(|nodes| !nodes.is_empty())
        {
            EdgeCaptureState::Running
        } else {
            EdgeCaptureState::Stopped
        }
    }

    pub fn attach_media_connection(
        &self,
        key: &str,
        owner: u64,
        sender: tokio::sync::mpsc::UnboundedSender<EdgeControlMessage>,
    ) {
        let previous = self
            .media_connections
            .lock()
            .insert(key.to_string(), EdgeMediaConnection { owner, sender });
        if let Some(previous) = previous {
            self.remove_media_connection(key, previous.owner, "Edge PCM接続が更新されました");
        }
    }

    fn remove_media_connection(&self, key: &str, owner: u64, reason: &str) {
        {
            let mut connections = self.media_connections.lock();
            if connections
                .get(key)
                .is_some_and(|connection| connection.owner == owner)
            {
                connections.remove(key);
            }
        }
        let pending: Vec<_> = {
            let mut pending = self.media_pending.lock();
            let ids: Vec<_> = pending
                .iter()
                .filter(|((pending_key, _), request)| pending_key == key && request.owner == owner)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        for request in pending {
            let _ = request.sender.send(Err(reason.to_string()));
        }
    }

    fn remove_media_pending(&self, key: &str, request_id: &str, owner: u64) {
        let mut pending = self.media_pending.lock();
        let id = (key.to_string(), request_id.to_string());
        if pending
            .get(&id)
            .is_some_and(|pending| pending.owner == owner)
        {
            pending.remove(&id);
        }
    }

    pub fn should_send_media_command(&self, key: &str, owner: u64, request_id: &str) -> bool {
        self.media_pending
            .lock()
            .get(&(key.to_string(), request_id.to_string()))
            .is_some_and(|pending| pending.owner == owner)
    }

    pub async fn control_media(
        &self,
        key: &str,
        action: EdgeMediaAction,
    ) -> Result<EdgeMediaResult> {
        let request_id = Uuid::new_v4().to_string();
        let (owner, sender) = self
            .media_connections
            .lock()
            .get(key)
            .map(|connection| (connection.owner, connection.sender.clone()))
            .ok_or_else(|| {
                BackendError::InvalidOperation("対象EdgeタブはPCM接続されていません".into())
            })?;
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        self.media_pending.lock().insert(
            (key.to_string(), request_id.clone()),
            EdgeMediaPending {
                owner,
                sender: result_sender,
            },
        );
        let mut pending_guard = EdgeMediaPendingGuard {
            hub: self.clone(),
            key: key.to_string(),
            request_id: request_id.clone(),
            owner,
            armed: true,
        };
        if sender
            .send(EdgeControlMessage::MediaCommand {
                stream_key: key.to_string(),
                request_id: request_id.clone(),
                action,
            })
            .is_err()
        {
            self.remove_media_connection(key, owner, "Edge PCM接続へ制御を送信できませんでした");
        }
        let response = match tokio::time::timeout(Duration::from_secs(3), result_receiver).await {
            Ok(Ok(Ok(response))) => response,
            Ok(Ok(Err(reason))) => return Err(BackendError::InvalidOperation(reason)),
            Ok(Err(_)) => {
                return Err(BackendError::InvalidOperation(
                    "Edgeメディア制御の応答を受信できませんでした".into(),
                ))
            }
            Err(_) => {
                return Err(BackendError::InvalidOperation(
                    "Edgeメディア制御がタイムアウトしました".into(),
                ));
            }
        };
        pending_guard.disarm();
        if response.error.is_some()
            || !matches!(
                response.state,
                EdgeMediaState::Playing | EdgeMediaState::Paused
            )
            || matches!(
                (action, response.state),
                (EdgeMediaAction::Play, EdgeMediaState::Paused)
                    | (EdgeMediaAction::Pause, EdgeMediaState::Playing)
            )
        {
            return Err(BackendError::InvalidOperation(
                response
                    .error
                    .clone()
                    .unwrap_or_else(|| "Edgeタブはメディア制御に対応していません".into()),
            ));
        }
        Ok(response)
    }

    pub fn resolve_media_result(&self, key: &str, owner: u64, result: EdgeMediaResult) {
        let pending = {
            let mut pending = self.media_pending.lock();
            let id = (key.to_string(), result.request_id.clone());
            if pending
                .get(&id)
                .is_some_and(|pending| pending.owner == owner)
            {
                pending.remove(&id)
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            if let Some(stream) = self.get(key) {
                stream.set_media_state(result.state, result.error.clone());
            }
            let _ = pending.sender.send(Ok(result));
        }
    }

    pub fn update_media_state(&self, key: &str, state: EdgeMediaState, error: Option<String>) {
        if let Some(stream) = self.get(key) {
            stream.set_media_state(state, error);
        }
    }

    pub fn push_pcm_f32le(&self, key: &str, bytes: &[u8]) -> Result<usize> {
        let stream = self.get(key).ok_or_else(|| {
            BackendError::InvalidOperation(format!("unknown Edge stream key: {key}"))
        })?;
        if stream.capture_state() == EdgeCaptureState::Stopped {
            return Ok(0);
        }
        if bytes.len() % 4 != 0 {
            return Err(BackendError::InvalidOperation(
                "Edge PCM frame length is not a multiple of 4".into(),
            ));
        }
        let mut raw = Vec::with_capacity(bytes.len() / 4);
        for chunk in bytes.chunks_exact(4) {
            raw.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        let rate = stream.sample_rate.load(Ordering::Relaxed);
        let channels = stream.channels.load(Ordering::Relaxed) as u16;
        if !(1..=2).contains(&channels) || rate == 0 || bytes.len() / 4 / channels as usize > 4096 {
            return Err(BackendError::InvalidOperation(
                "invalid Edge PCM frame".into(),
            ));
        }
        // Start threshold (target*0.5) is the steady operating fill. Keep a
        // deadband around it at ratio=1 so ordinary queue jitter never alters
        // PCM. Sustained drift correction is added separately after measuring
        // the Edge producer/consumer clocks.
        let mut stereo = Vec::with_capacity(raw.len() * if channels == 1 { 2 } else { 1 });
        if channels == 1 {
            for sample in raw {
                stereo.extend([sample, sample]);
            }
        } else {
            stereo = raw;
        }
        let ratio = INTERNAL_SAMPLE_RATE as f64 / rate as f64
            * stream.correction_ratio(stereo.len() / 2, rate);
        let converted = stream.resampler.lock().process(&stereo, ratio)?;
        stream.heartbeat();
        Ok(stream.jitter.push(&converted))
    }
}

pub struct EdgePcmLease {
    hub: EdgeHub,
    key: String,
    owner: u64,
}
impl EdgePcmLease {
    pub(crate) fn owner(&self) -> u64 {
        self.owner
    }
}
impl Drop for EdgePcmLease {
    fn drop(&mut self) {
        self.hub.release_pcm_lease(&self.key, self.owner);
    }
}

#[cfg(test)]
mod tests {
    use super::{EdgeHub, EdgeMediaResult, EdgeStreamState};
    use crate::{
        audio::graph::NodeId,
        edge::control::{EdgeCaptureState, EdgeControlMessage, EdgeMediaAction, EdgeMediaState},
    };
    use std::time::Duration;

    #[test]
    fn edge_auth_token_can_be_rotated_and_old_token_is_rejected() {
        let hub = EdgeHub::default();
        let old = hub.auth_token();
        assert!(hub.authenticate(Some(&old), 1).is_ok());
        let next = hub.regenerate_auth_token();
        assert_ne!(old, next);
        assert_eq!(hub.authenticate(Some(&old), 1), Err("bad_token"));
        assert_eq!(hub.authenticate(Some(&next), 2), Err("version"));
        assert!(hub.authenticate(Some(&next), 1).is_ok());
    }

    #[test]
    fn edge_stream_keeps_tab_metadata_across_reconnect() {
        let hub = EdgeHub::default();
        let stream = hub.register("tab", 48_000, 2);
        stream.update_tab(Some(42), Some("Example"), Some("https://example.test"));
        hub.remove("tab");
        let same = hub.register("tab", 48_000, 2);
        assert!(std::sync::Arc::ptr_eq(&stream, &same));
        assert_eq!(same.tab_id(), Some(42));
        assert_eq!(same.title(), "Example");
        assert_eq!(same.url(), "https://example.test");
        assert_eq!(same.state(Duration::from_secs(1)), EdgeStreamState::Running);
        assert_eq!(hub.list_tabs()[0].tab_id, Some(42));
        same.update_tab(None, Some("Updated"), None);
        assert_eq!(same.tab_id(), Some(42));
    }

    #[test]
    fn receiver_enablement_is_counted_per_node_and_stream() {
        let hub = EdgeHub::default();
        let first = NodeId::new();
        let second = NodeId::new();
        hub.set_receiver_enabled("same", first, true);
        hub.set_receiver_enabled("same", second, true);
        hub.set_receiver_enabled("other", NodeId::new(), true);
        assert_eq!(hub.receiver_state("same"), EdgeCaptureState::Running);
        assert_eq!(hub.receiver_state("other"), EdgeCaptureState::Running);
        hub.set_receiver_enabled("same", first, false);
        assert_eq!(hub.receiver_state("same"), EdgeCaptureState::Running);
        hub.set_receiver_enabled("same", second, false);
        assert_eq!(hub.receiver_state("same"), EdgeCaptureState::Stopped);
        assert_eq!(hub.receiver_state("other"), EdgeCaptureState::Running);
    }

    #[test]
    fn removing_deleted_node_clears_every_stream_without_disabling_peers() {
        let hub = EdgeHub::default();
        let deleted = NodeId::new();
        let peer = NodeId::new();
        hub.set_receiver_enabled("first", deleted, true);
        hub.set_receiver_enabled("second", deleted, true);
        hub.set_receiver_enabled("second", peer, true);

        hub.remove_receiver_node(deleted);

        assert_eq!(hub.receiver_state("first"), EdgeCaptureState::Stopped);
        assert_eq!(hub.receiver_state("second"), EdgeCaptureState::Running);
    }

    #[test]
    fn capture_pause_ignores_pcm_and_does_not_affect_other_streams() {
        let hub = EdgeHub::default();
        hub.register("paused", 48_000, 2);
        hub.register("other", 48_000, 2);
        hub.set_capture_state("paused", EdgeCaptureState::Stopped)
            .unwrap();
        assert_eq!(
            hub.push_pcm_f32le("paused", &[0; 8]).unwrap(),
            0,
            "intentional pause must discard PCM"
        );
        assert_eq!(
            hub.get("paused").unwrap().state(Duration::from_secs(1)),
            EdgeStreamState::Stopped
        );
        assert_eq!(
            hub.get("other").unwrap().state(Duration::from_secs(1)),
            EdgeStreamState::Running
        );
        hub.set_capture_state("paused", EdgeCaptureState::Running)
            .unwrap();
        assert_eq!(
            hub.get("paused").unwrap().state(Duration::from_secs(1)),
            EdgeStreamState::Running
        );
    }

    #[tokio::test]
    async fn media_timeout_removes_pending_request_before_queued_command_can_send() {
        let hub = EdgeHub::default();
        let lease = hub.acquire_pcm_lease("timeout", 48_000, 2).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        hub.attach_media_connection("timeout", lease.owner(), sender);
        let requested_hub = hub.clone();
        let request = tokio::spawn(async move {
            requested_hub
                .control_media("timeout", EdgeMediaAction::Play)
                .await
        });
        let EdgeControlMessage::MediaCommand { request_id, .. } = receiver.recv().await.unwrap()
        else {
            panic!("expected media command");
        };
        let error = request.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("タイムアウト"));
        assert!(!hub.should_send_media_command("timeout", lease.owner(), &request_id));
    }

    #[tokio::test]
    async fn aborting_media_request_removes_pending_command() {
        let hub = EdgeHub::default();
        let lease = hub.acquire_pcm_lease("abort", 48_000, 2).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        hub.attach_media_connection("abort", lease.owner(), sender);
        let requested_hub = hub.clone();
        let request = tokio::spawn(async move {
            requested_hub
                .control_media("abort", EdgeMediaAction::Pause)
                .await
        });
        let EdgeControlMessage::MediaCommand { request_id, .. } = receiver.recv().await.unwrap()
        else {
            panic!("expected media command");
        };
        request.abort();
        let _ = request.await;
        assert!(!hub.should_send_media_command("abort", lease.owner(), &request_id));
    }

    #[tokio::test]
    async fn owner_mismatch_does_not_remove_the_real_media_request() {
        let hub = EdgeHub::default();
        let lease = hub.acquire_pcm_lease("owner", 48_000, 2).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        hub.attach_media_connection("owner", lease.owner(), sender);
        let requested_hub = hub.clone();
        let request = tokio::spawn(async move {
            requested_hub
                .control_media("owner", EdgeMediaAction::Pause)
                .await
        });
        let EdgeControlMessage::MediaCommand { request_id, .. } = receiver.recv().await.unwrap()
        else {
            panic!("expected media command");
        };
        let result = EdgeMediaResult {
            stream_key: "owner".into(),
            request_id: request_id.clone(),
            state: EdgeMediaState::Paused,
            error: None,
        };
        hub.resolve_media_result("owner", lease.owner() + 1, result.clone());
        assert!(hub.should_send_media_command("owner", lease.owner(), &request_id));
        assert!(!request.is_finished());
        hub.resolve_media_result("owner", lease.owner(), result);
        assert_eq!(
            request.await.unwrap().unwrap().state,
            EdgeMediaState::Paused
        );
    }

    #[test]
    fn edge_pcm_rejects_oversized_frame() {
        let hub = EdgeHub::default();
        hub.register("tab", 48_000, 2);
        let bytes = vec![0u8; 4097 * 2 * 4];
        assert!(hub.push_pcm_f32le("tab", &bytes).is_err());
    }

    #[test]
    fn pcm_lease_rejects_duplicate_and_releases_on_drop() {
        let hub = EdgeHub::default();
        let first = hub.acquire_pcm_lease("tab", 48_000, 2).unwrap();
        assert!(matches!(
            hub.acquire_pcm_lease("tab", 48_000, 2),
            Err("stream_in_use")
        ));
        drop(first);
        let second = hub.acquire_pcm_lease("tab", 48_000, 2).unwrap();
        drop(second);
        assert_eq!(
            hub.get("tab").unwrap().state(Duration::from_secs(1)),
            EdgeStreamState::Disconnected
        );
    }

    #[test]
    fn reconnect_reset_drops_previous_lease_pcm() {
        let hub = EdgeHub::default();
        let first = hub.acquire_pcm_lease("tab", 48_000, 2).unwrap();
        let old = 1.0f32.to_le_bytes();
        let new = 0.25f32.to_le_bytes();
        let old_packet: Vec<u8> = old.into_iter().cycle().take(480 * 2 * 4).collect();
        let new_packet: Vec<u8> = new.into_iter().cycle().take(480 * 2 * 4).collect();
        for _ in 0..60 {
            hub.push_pcm_f32le("tab", &old_packet).unwrap();
        }
        let stream = hub.get("tab").unwrap();
        let mut old_output = vec![0.0; 480 * 2];
        assert_eq!(stream.jitter.read_into(&mut old_output), old_output.len());
        assert!(old_output.iter().all(|sample| *sample == 1.0));
        drop(first);
        let second = hub.acquire_pcm_lease("tab", 48_000, 2).unwrap();
        for _ in 0..60 {
            hub.push_pcm_f32le("tab", &new_packet).unwrap();
        }
        let mut output = vec![0.0; 480 * 2];
        assert_eq!(stream.jitter.read_into(&mut output), output.len());
        assert!(output.iter().all(|sample| *sample == 0.25));
        drop(second);
    }
}
