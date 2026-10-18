use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use arc_swap::ArcSwap;
use chrono::Utc;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};

use crate::{
    audio::graph::{GraphSnapshot, NodeId, NodeProperties},
    error::{BackendError, Result},
    event_graph::{EventDelivery, EventEnvelope},
};

use super::{
    hotkey::{HotkeyExecutor, KeyInputSender},
    http,
    model::{
        ActionConfig, ActionConnectionState, ActionExecutionId, ActionExecutionResult,
        ActionNodeConfig, ActionRuntimeEvent, ActionRuntimeState, LogOutputConfig, LogOutputFormat,
        ObsConnectionConfig, ObsConnectionId, SecretId,
    },
    obs,
    secrets::SecretStore,
};

const ACTION_QUEUE_CAPACITY: usize = 256;
const ACTION_WORKERS: usize = 3;

#[derive(Clone)]
enum JobConfig {
    Action(ActionNodeConfig),
    Log(LogOutputConfig),
}

struct ActionJob {
    execution_id: ActionExecutionId,
    node_id: NodeId,
    event: Option<EventEnvelope>,
    config: JobConfig,
}

pub struct ActionEngine {
    graph: Arc<ArcSwap<GraphSnapshot>>,
    deliveries: Receiver<EventDelivery>,
    jobs_tx: Sender<ActionJob>,
    jobs_rx: Receiver<ActionJob>,
    events_tx: Sender<ActionRuntimeEvent>,
    events_rx: Receiver<ActionRuntimeEvent>,
    states: Arc<RwLock<HashMap<NodeId, ActionRuntimeState>>>,
    node_locks: Arc<RwLock<HashMap<NodeId, Arc<Mutex<()>>>>>,
    obs_connections: Arc<RwLock<HashMap<ObsConnectionId, ObsConnectionConfig>>>,
    secrets: SecretStore,
    hotkey: Arc<HotkeyExecutor>,
    handle: Mutex<Option<ActionEngineHandle>>,
}

impl ActionEngine {
    pub fn new(
        graph: Arc<ArcSwap<GraphSnapshot>>,
        deliveries: Receiver<EventDelivery>,
        obs_connections: HashMap<ObsConnectionId, ObsConnectionConfig>,
    ) -> Self {
        Self::new_with_hotkey_sender(graph, deliveries, obs_connections, None)
    }

    pub fn new_with_hotkey_sender(
        graph: Arc<ArcSwap<GraphSnapshot>>,
        deliveries: Receiver<EventDelivery>,
        obs_connections: HashMap<ObsConnectionId, ObsConnectionConfig>,
        sender: Option<Arc<dyn KeyInputSender>>,
    ) -> Self {
        let (jobs_tx, jobs_rx) = crossbeam_channel::bounded(ACTION_QUEUE_CAPACITY);
        let (events_tx, events_rx) = crossbeam_channel::bounded(1024);
        Self {
            graph,
            deliveries,
            jobs_tx,
            jobs_rx,
            events_tx,
            events_rx,
            states: Arc::new(RwLock::new(HashMap::new())),
            node_locks: Arc::new(RwLock::new(HashMap::new())),
            obs_connections: Arc::new(RwLock::new(obs_connections)),
            secrets: SecretStore::default(),
            hotkey: Arc::new(match sender {
                Some(sender) => HotkeyExecutor::new(sender),
                None => HotkeyExecutor::platform(),
            }),
            handle: Mutex::new(None),
        }
    }

    pub fn start(&self) -> Result<()> {
        let mut handle = self.handle.lock();
        if handle.is_some() {
            return Ok(());
        }
        while self.jobs_rx.try_recv().is_ok() {}
        while self.deliveries.try_recv().is_ok() {}
        let stop = Arc::new(AtomicBool::new(false));
        let coordinator_stop = stop.clone();
        let graph = self.graph.clone();
        let deliveries = self.deliveries.clone();
        let jobs = self.jobs_tx.clone();
        let event_sender = self.events_tx.clone();
        let states = self.states.clone();
        let coordinator = thread::Builder::new()
            .name("action-coordinator".into())
            .spawn(move || {
                while !coordinator_stop.load(Ordering::Acquire) {
                    let Ok(delivery) = deliveries.recv_timeout(Duration::from_millis(20)) else {
                        continue;
                    };
                    let Some(config) = graph
                        .load()
                        .nodes
                        .get(&delivery.target_node_id)
                        .and_then(job_config)
                    else {
                        continue;
                    };
                    if matches!(&config, JobConfig::Action(config) if !config.enabled) {
                        continue;
                    }
                    let job = ActionJob {
                        execution_id: ActionExecutionId::new(),
                        node_id: delivery.target_node_id,
                        event: Some(delivery.envelope),
                        config,
                    };
                    if let Err(error) = jobs.try_send(job) {
                        report_queue_full(error.into_inner(), &event_sender, &states);
                    }
                }
            })?;

        let mut workers = Vec::with_capacity(ACTION_WORKERS);
        for index in 0..ACTION_WORKERS {
            let worker_stop = stop.clone();
            let jobs = self.jobs_rx.clone();
            let retry_jobs = self.jobs_tx.clone();
            let events = self.events_tx.clone();
            let states = self.states.clone();
            let node_locks = self.node_locks.clone();
            let obs_connections = self.obs_connections.clone();
            let secrets = self.secrets.clone();
            let hotkey = self.hotkey.clone();
            workers.push(
                thread::Builder::new()
                    .name(format!("action-worker-{index}"))
                    .spawn(move || {
                        while !worker_stop.load(Ordering::Acquire) {
                            let Ok(job) = jobs.recv_timeout(Duration::from_millis(20)) else {
                                continue;
                            };
                            let node_lock = {
                                let mut locks = node_locks.write();
                                locks
                                    .entry(job.node_id)
                                    .or_insert_with(|| Arc::new(Mutex::new(())))
                                    .clone()
                            };
                            let Some(_node_serialization) = node_lock.try_lock() else {
                                if let Err(error) = retry_jobs.try_send(job) {
                                    report_queue_full(error.into_inner(), &events, &states);
                                } else {
                                    // Yield this worker immediately so a job for another
                                    // node can run instead of blocking behind this node.
                                    thread::sleep(Duration::from_millis(1));
                                }
                                continue;
                            };
                            execute_job(job, &events, &states, &obs_connections, &secrets, &hotkey);
                        }
                    })?,
            );
        }
        *handle = Some(ActionEngineHandle {
            stop,
            coordinator: Some(coordinator),
            workers,
        });
        Ok(())
    }

    pub fn stop(&self) {
        self.handle.lock().take();
    }

    pub fn event_receiver(&self) -> Receiver<ActionRuntimeEvent> {
        self.events_rx.clone()
    }

    pub fn runtime_state(&self, node_id: NodeId) -> ActionRuntimeState {
        self.states
            .read()
            .get(&node_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn execute_test(&self, node_id: NodeId) -> Result<ActionExecutionId> {
        self.start()?;
        let node = self
            .graph
            .load()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        let NodeProperties::Action { config } = node.properties else {
            return Err(BackendError::InvalidOperation(
                "node is not an Action node".into(),
            ));
        };
        config.validate().map_err(BackendError::InvalidOperation)?;
        let execution_id = ActionExecutionId::new();
        self.jobs_tx
            .try_send(ActionJob {
                execution_id,
                node_id,
                event: None,
                config: JobConfig::Action(config.clone()),
            })
            .map_err(|_| BackendError::InvalidOperation("Action queue is full".into()))?;
        Ok(execution_id)
    }

    pub fn set_secret(&self, id: Option<SecretId>, value: String) -> SecretId {
        let id = id.unwrap_or_default();
        self.secrets.set(id, value);
        id
    }

    pub fn set_secret_persistent(&self, id: Option<SecretId>, value: String) -> Result<SecretId> {
        let id = id.unwrap_or_default();
        self.secrets
            .set_persistent(id, value)
            .map_err(BackendError::InvalidOperation)?;
        Ok(id)
    }

    pub fn delete_secret(&self, id: SecretId) {
        self.secrets.remove(id);
    }

    pub fn delete_secret_persistent(&self, id: SecretId) -> Result<()> {
        self.secrets
            .remove_persistent(id)
            .map_err(BackendError::InvalidOperation)?;
        Ok(())
    }

    pub fn upsert_obs_connection(
        &self,
        config: ObsConnectionConfig,
    ) -> Result<ObsConnectionConfig> {
        config.validate().map_err(BackendError::InvalidOperation)?;
        self.obs_connections
            .write()
            .insert(config.id, config.clone());
        Ok(config)
    }

    pub fn delete_obs_connection(&self, id: ObsConnectionId) -> Result<()> {
        self.obs_connections
            .write()
            .remove(&id)
            .ok_or_else(|| BackendError::InvalidOperation("OBS connection not found".into()))?;
        Ok(())
    }

    pub fn list_obs_connections(&self) -> Vec<ObsConnectionConfig> {
        let mut values: Vec<_> = self.obs_connections.read().values().cloned().collect();
        values.sort_by_key(|config| config.id);
        values
    }

    pub fn obs_connections(&self) -> HashMap<ObsConnectionId, ObsConnectionConfig> {
        self.obs_connections.read().clone()
    }

    pub fn replace_obs_connections(
        &self,
        connections: HashMap<ObsConnectionId, ObsConnectionConfig>,
    ) -> Result<()> {
        for connection in connections.values() {
            connection
                .validate()
                .map_err(BackendError::InvalidOperation)?;
        }
        *self.obs_connections.write() = connections;
        Ok(())
    }
}

fn job_config(node: &crate::audio::graph::NodeSpec) -> Option<JobConfig> {
    match &node.properties {
        NodeProperties::Action { config } => Some(JobConfig::Action(config.clone())),
        NodeProperties::LogOutput { config } => Some(JobConfig::Log(config.clone())),
        _ => None,
    }
}

fn execute_job(
    job: ActionJob,
    events: &Sender<ActionRuntimeEvent>,
    states: &RwLock<HashMap<NodeId, ActionRuntimeState>>,
    obs_connections: &RwLock<HashMap<ObsConnectionId, ObsConnectionConfig>>,
    secrets: &SecretStore,
    hotkey: &HotkeyExecutor,
) {
    let started_at = Utc::now();
    let event_id = job.event.as_ref().map(|event| event.event_id);
    states.write().entry(job.node_id).or_default().running = true;
    let _ = events.try_send(ActionRuntimeEvent::ExecutionStarted {
        execution_id: job.execution_id,
        node_id: job.node_id,
        event_id,
        started_at,
    });

    let (outcome, status_code) = match &job.config {
        JobConfig::Action(config) => {
            let _ = events.try_send(ActionRuntimeEvent::ConnectionStateChanged {
                node_id: job.node_id,
                state: ActionConnectionState::Connecting,
            });
            let result = match &config.action {
                ActionConfig::Http(http_config) => {
                    http::execute(http_config, job.event.as_ref(), secrets, config.timeout_ms)
                        .map(|outcome| Some(outcome.status_code))
                }
                ActionConfig::Obs(obs_config) => obs_connections
                    .read()
                    .get(&obs_config.connection_id)
                    .cloned()
                    .ok_or_else(|| "OBS_NOT_CONNECTED: connection is not configured".to_string())
                    .and_then(|connection| {
                        obs::execute(obs_config, &connection, secrets, config.timeout_ms)
                    })
                    .map(|_| None),
                ActionConfig::Hotkey(hotkey_config) => hotkey.execute(hotkey_config).map(|_| None),
            };
            match result {
                Ok(status_code) => (Ok(()), status_code),
                Err(message) => (Err(message), None),
            }
        }
        JobConfig::Log(config) => (write_log(config, job.event.as_ref()), None),
    };
    let success = outcome.is_ok();
    let message = outcome.err();
    let result = ActionExecutionResult {
        execution_id: job.execution_id,
        node_id: job.node_id,
        event_id,
        started_at,
        finished_at: Utc::now(),
        success,
        message,
        status_code,
    };
    let connection_state = if success {
        ActionConnectionState::Connected
    } else {
        ActionConnectionState::Error
    };
    states.write().insert(
        job.node_id,
        ActionRuntimeState {
            connection_state,
            running: false,
            last_result: Some(result.clone()),
        },
    );
    let _ = events.try_send(ActionRuntimeEvent::ConnectionStateChanged {
        node_id: job.node_id,
        state: connection_state,
    });
    let _ = events.try_send(ActionRuntimeEvent::ExecutionFinished {
        result: result.clone(),
    });
}

fn write_log(
    config: &LogOutputConfig,
    event: Option<&EventEnvelope>,
) -> std::result::Result<(), String> {
    config.validate()?;
    let event = event.ok_or_else(|| "LOG_OUTPUT_PATH_UNWRITABLE: event is missing".to_string())?;
    let mut line = match config.format {
        LogOutputFormat::PlainText => format!(
            "{} {} {} {}",
            event.timestamp_utc.to_rfc3339(),
            event.event_type.replace(['\r', '\n'], " "),
            event.source_node_id,
            event.payload.to_string().replace(['\r', '\n'], " ")
        ),
        LogOutputFormat::JsonLines => serde_json::to_string(event)
            .map_err(|_| "LOG_OUTPUT_PATH_UNWRITABLE: serialization failed".to_string())?,
    };
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.path)
        .map_err(|_| "LOG_OUTPUT_PATH_UNWRITABLE: file open failed".to_string())?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|_| "LOG_OUTPUT_PATH_UNWRITABLE: file write failed".to_string())
}

fn report_queue_full(
    job: ActionJob,
    events: &Sender<ActionRuntimeEvent>,
    states: &RwLock<HashMap<NodeId, ActionRuntimeState>>,
) {
    let now = Utc::now();
    let result = ActionExecutionResult {
        execution_id: job.execution_id,
        node_id: job.node_id,
        event_id: job.event.as_ref().map(|event| event.event_id),
        started_at: now,
        finished_at: now,
        success: false,
        message: Some("ACTION_QUEUE_FULL: Action queue is full".into()),
        status_code: None,
    };
    states.write().insert(
        job.node_id,
        ActionRuntimeState {
            connection_state: ActionConnectionState::Error,
            running: false,
            last_result: Some(result.clone()),
        },
    );
    let _ = events.try_send(ActionRuntimeEvent::ExecutionFinished { result });
}

struct ActionEngineHandle {
    stop: Arc<AtomicBool>,
    coordinator: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl Drop for ActionEngineHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(coordinator) = self.coordinator.take() {
            let _ = coordinator.join();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for ActionEngine {
    fn drop(&mut self) {
        self.handle.get_mut().take();
    }
}
