use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    audio::graph::{EdgeId, GraphSnapshot, NodeId, PortId},
    Result,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct EventId(pub Uuid);

impl EventId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub event_id: EventId,
    pub source_node_id: NodeId,
    pub event_type: String,
    pub timestamp_utc: DateTime<Utc>,
    pub monotonic_timestamp_ns: u64,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceTransitionKind {
    Started,
    Resumed,
}

#[derive(Debug, Clone, Copy)]
pub enum PendingEvent {
    Silence {
        source_node_id: NodeId,
        source_port_id: PortId,
        kind: SilenceTransitionKind,
        level_db: f32,
        silent_duration_ms: u64,
    },
}

impl PendingEvent {
    fn source(&self) -> (NodeId, PortId) {
        match *self {
            Self::Silence {
                source_node_id,
                source_port_id,
                ..
            } => (source_node_id, source_port_id),
        }
    }

    fn into_envelope(self, started: Instant) -> EventEnvelope {
        match self {
            Self::Silence {
                source_node_id,
                kind,
                level_db,
                silent_duration_ms,
                ..
            } => EventEnvelope {
                event_id: EventId::new(),
                source_node_id,
                event_type: match kind {
                    SilenceTransitionKind::Started => "audio.silence.started",
                    SilenceTransitionKind::Resumed => "audio.silence.resumed",
                }
                .into(),
                timestamp_utc: Utc::now(),
                monotonic_timestamp_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                payload: json!({
                    "level_db": level_db,
                    "silent_duration_ms": silent_duration_ms,
                }),
            },
        }
    }
}

#[derive(Clone)]
pub struct EventIngress(Sender<PendingEvent>);

impl EventIngress {
    /// Realtime-safe enqueue. A full queue drops the event instead of blocking
    /// the audio callback.
    pub fn try_send(&self, event: PendingEvent) -> bool {
        self.0.try_send(event).is_ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventDelivery {
    pub edge_id: EdgeId,
    pub target_node_id: NodeId,
    pub target_port_id: PortId,
    pub envelope: EventEnvelope,
}

pub struct EventEngine {
    graph: Arc<ArcSwap<GraphSnapshot>>,
    ingress: EventIngress,
    ingress_rx: Receiver<PendingEvent>,
    events_tx: Sender<EventEnvelope>,
    events_rx: Receiver<EventEnvelope>,
    deliveries_tx: Sender<EventDelivery>,
    deliveries_rx: Receiver<EventDelivery>,
    targets: Arc<RwLock<HashMap<NodeId, Vec<Sender<EventDelivery>>>>>,
    handle: Mutex<Option<EventEngineHandle>>,
}

impl EventEngine {
    pub fn new(graph: Arc<ArcSwap<GraphSnapshot>>) -> Self {
        let (ingress_tx, ingress_rx) = crossbeam_channel::bounded(1024);
        let (events_tx, events_rx) = crossbeam_channel::bounded(1024);
        let (deliveries_tx, deliveries_rx) = crossbeam_channel::bounded(1024);
        Self {
            graph,
            ingress: EventIngress(ingress_tx),
            ingress_rx,
            events_tx,
            events_rx,
            deliveries_tx,
            deliveries_rx,
            targets: Arc::new(RwLock::new(HashMap::new())),
            handle: Mutex::new(None),
        }
    }

    pub fn ingress(&self) -> EventIngress {
        self.ingress.clone()
    }

    pub fn event_receiver(&self) -> Receiver<EventEnvelope> {
        self.events_rx.clone()
    }

    /// Single control-plane stream consumed by the Action coordinator. This is
    /// separate from frontend envelopes so neither consumer steals messages.
    pub fn delivery_receiver(&self) -> Receiver<EventDelivery> {
        self.deliveries_rx.clone()
    }

    pub fn subscribe_target(&self, node_id: NodeId) -> Receiver<EventDelivery> {
        let (sender, receiver) = crossbeam_channel::bounded(256);
        self.targets
            .write()
            .entry(node_id)
            .or_default()
            .push(sender);
        receiver
    }

    pub fn start(&self) -> Result<()> {
        let mut handle = self.handle.lock();
        if handle.is_some() {
            return Ok(());
        }
        while self.ingress_rx.try_recv().is_ok() {}
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let graph = self.graph.clone();
        let ingress = self.ingress_rx.clone();
        let events = self.events_tx.clone();
        let deliveries = self.deliveries_tx.clone();
        let targets = self.targets.clone();
        let join = thread::Builder::new()
            .name("event-graph-runtime".into())
            .spawn(move || {
                let started = Instant::now();
                while !thread_stop.load(Ordering::Acquire) {
                    let Ok(pending) = ingress.recv_timeout(Duration::from_millis(20)) else {
                        continue;
                    };
                    let (source_node_id, source_port_id) = pending.source();
                    let envelope = pending.into_envelope(started);
                    let _ = events.try_send(envelope.clone());
                    let snapshot = graph.load();
                    for edge in snapshot.event_edges.iter().filter(|edge| {
                        edge.from_node == source_node_id && edge.from_port == source_port_id
                    }) {
                        let delivery = EventDelivery {
                            edge_id: edge.id,
                            target_node_id: edge.to_node,
                            target_port_id: edge.to_port,
                            envelope: envelope.clone(),
                        };
                        let _ = deliveries.try_send(delivery.clone());
                        if let Some(subscribers) = targets.read().get(&edge.to_node) {
                            for subscriber in subscribers {
                                let _ = subscriber.try_send(delivery.clone());
                            }
                        }
                    }
                }
            })?;
        *handle = Some(EventEngineHandle {
            stop,
            join: Some(join),
        });
        Ok(())
    }

    pub fn stop(&self) {
        self.handle.lock().take();
    }
}

struct EventEngineHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for EventEngineHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for EventEngine {
    fn drop(&mut self) {
        self.handle.get_mut().take();
    }
}
