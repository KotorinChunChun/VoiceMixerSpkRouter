pub mod actions;
pub mod audio;
pub mod codex_config;
pub mod commands;
pub mod config;
pub mod contracts;
pub mod controller;
pub mod edge;
pub mod error;
pub mod event_graph;
pub mod events;
pub mod nodes;
pub mod plugins;
pub mod profile;
pub mod recording;
pub mod registry;
pub mod replay;
pub mod routing;
pub mod runtime_manager;
pub mod rvc;
pub mod shortcut;
pub mod speech_cli;
pub mod standalone;
pub mod validation;
pub mod window_state;
pub mod windows;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use controller::GraphController;
pub use error::{BackendError, Result};

use std::{collections::HashMap, sync::Arc};

use actions::ActionEngine;
use audio::engine::{AudioRuntime, RuntimeHandle, RuntimeSettings};
use config::persistence::ConfigStore;
use config::routing_profile;
use edge::receiver::EdgeHub;
use event_graph::EventEngine;
use events::BackendEvent;
use parking_lot::Mutex;
use parking_lot::RwLock;
use plugins::{
    create_default_plugin_manager,
    manager::PluginManager,
    package::{discover_process_plugins, PluginDiscoveryReport},
};
use registry::{
    device_history::DeviceHistory,
    node_registry::{NodeRegistry, RegisteredNode},
};
use replay::cache::ReplayCacheManager;
use runtime_manager::{NodeRuntimeManager, NodeStartResult};
use rvc::manager::RvcManager;
use shortcut::{
    dispatcher::{spawn_dispatcher, ShortcutDispatchHandle},
    model::KeyCombination,
    registrar::ShortcutRegistrar,
    ShortcutEngine,
};

/// Top-level backend object intended to be stored in Tauri managed state.
///
/// UI code changes graph/configuration through `GraphController`; the realtime
/// runtime only receives immutable graph snapshots at block boundaries.
pub struct Backend {
    pub speech_cli: speech_cli::SpeechSession,
    pub codex_config: codex_config::CodexSession,
    /// 設定変更Commandの複合操作を直列化する。内部Command呼出しは同一threadで再入可能。
    pub(crate) operation_lock: parking_lot::ReentrantMutex<()>,
    pub controller: Arc<GraphController>,
    pub runtime: Arc<AudioRuntime>,
    pub event_engine: Arc<EventEngine>,
    pub action_engine: Arc<ActionEngine>,
    pub shortcut_engine: Arc<ShortcutEngine>,
    pub config: ConfigStore,
    pub registry: Arc<NodeRegistry>,
    pub device_history: Arc<RwLock<DeviceHistory>>,
    pub replay_caches: Arc<ReplayCacheManager>,
    pub edge: EdgeHub,
    pub rvc: Arc<RvcManager>,
    pub plugins: Arc<PluginManager>,
    pub plugin_discovery: Mutex<PluginDiscoveryReport>,
    pub nodes: Arc<NodeRuntimeManager>,
    pub active_routing_profile: Mutex<Option<String>>,
    pub completed_playback_device_operations: Mutex<HashMap<uuid::Uuid, audio::graph::NodeId>>,
    pub input_monitor: windows::input_monitor::InputMonitorManager,
    pub audition_render: Mutex<Option<windows::wasapi_render::RenderHandle>>,
    pub voice_models: Mutex<Option<plugins::voice_models::VoiceModelManager>>,
    pub meter_monitor: Arc<windows::meter_monitor::MeterMonitor>,
    pending_events: Mutex<Vec<BackendEvent>>,
    runtime_handle: Mutex<Option<RuntimeHandle>>,
    shortcut_dispatch_handle: Mutex<Option<ShortcutDispatchHandle>>,
}

impl Backend {
    pub fn new(config: ConfigStore) -> Result<Self> {
        Self::new_inner(config, None)
    }

    pub fn new_with_shortcut_registrar(
        config: ConfigStore,
        reserved: Vec<KeyCombination>,
        registrar: Box<dyn ShortcutRegistrar>,
    ) -> Result<Self> {
        Self::new_inner(config, Some((reserved, registrar)))
    }

    fn new_inner(
        config: ConfigStore,
        shortcut_override: Option<(Vec<KeyCombination>, Box<dyn ShortcutRegistrar>)>,
    ) -> Result<Self> {
        crate::config::transaction::recover(&config.base_dir())?;
        routing_profile::ensure_samples(&config)?;
        let controller = Arc::new(GraphController::new());
        let mut persisted = config.load()?;
        if persisted.settings.edge_auth_token.trim().is_empty() {
            persisted.settings.edge_auth_token = uuid::Uuid::new_v4().to_string();
            config.save(&persisted)?;
        }
        if !persisted.nodes.is_empty()
            || !persisted.routing_edges.is_empty()
            || !persisted.event_edges.is_empty()
        {
            // 配置直後・Undo後の未接続プラグインも編集状態として再読込する。
            // 実行開始時の必須入力検証はAudioRuntimeに残す。
            controller.commit_graph_candidate(
                0,
                audio::graph::GraphSnapshot {
                    revision: 0,
                    nodes: persisted.nodes.clone(),
                    edges: persisted.routing_edges.clone(),
                    event_edges: persisted.event_edges.clone(),
                    layouts: persisted.node_layouts.clone(),
                },
                |_| Ok(()),
            )?;
        }
        persisted
            .settings
            .validate()
            .map_err(BackendError::InvalidOperation)?;
        let event_engine = Arc::new(EventEngine::new(controller.runtime_snapshot()));
        let action_engine = Arc::new(ActionEngine::new(
            controller.runtime_snapshot(),
            event_engine.delivery_receiver(),
            persisted.obs_connections.clone(),
        ));
        let shortcut_engine = Arc::new(match shortcut_override {
            Some((reserved, registrar)) => ShortcutEngine::new_with_registrar(
                persisted.shortcuts.clone(),
                reserved,
                registrar,
            )?,
            None => ShortcutEngine::new(persisted.shortcuts.clone())?,
        });
        let runtime = Arc::new(AudioRuntime::new(
            controller.runtime_snapshot(),
            RuntimeSettings {
                block_frames: persisted.settings.block_frames,
                meter_fps: persisted.settings.meter_fps,
            },
        ));
        runtime.bind_event_ingress(event_engine.ingress());
        let edge = EdgeHub::default();
        edge.set_auth_token(persisted.settings.edge_auth_token.clone());
        let rvc = Arc::new(RvcManager::default());
        let plugins = create_default_plugin_manager()?;
        let search_paths: Vec<_> = persisted
            .settings
            .plugin_search_paths
            .iter()
            .map(|path| config.resolve_relative(path))
            .collect();
        let plugin_discovery = discover_process_plugins(&plugins, &search_paths);
        let replay_caches = Arc::new(ReplayCacheManager::new(
            config.base_dir().join("replay-cache"),
            persisted.replay_cache_metadata.clone(),
        ));
        let nodes = Arc::new(NodeRuntimeManager::new(
            controller.clone(),
            runtime.clone(),
            plugins.clone(),
            rvc.clone(),
            edge.clone(),
            replay_caches.clone(),
        ));
        nodes.set_latency_io(crate::runtime_manager::LatencyIoConfig {
            auto_capture_priority: persisted.settings.auto_capture_priority.clone(),
            render_exclusive: persisted.settings.render_exclusive,
        });
        edge.set_jitter_target_fill(persisted.settings.jitter_target_fill);
        let registered_nodes = if persisted.registered_nodes.is_empty() {
            persisted
                .nodes
                .values()
                .map(RegisteredNode::from_graph_node)
                .map(|node| (node.id, node))
                .collect()
        } else {
            persisted.registered_nodes.clone()
        };
        let registered_nodes = registered_nodes
            .into_iter()
            .map(|(id, mut node)| {
                node.instance_policy =
                    crate::registry::instance_policy::InstancePolicy::migrate_persisted(
                        &node.properties,
                        node.instance_policy,
                    );
                (id, node)
            })
            .collect();
        let registry = Arc::new(NodeRegistry::from_nodes(registered_nodes));
        let device_history = Arc::new(RwLock::new(persisted.device_history));
        Ok(Self {
            operation_lock: parking_lot::ReentrantMutex::new(()),
            controller,
            runtime,
            event_engine,
            action_engine,
            shortcut_engine,
            config,
            registry,
            device_history,
            replay_caches,
            edge,
            rvc,
            plugins,
            plugin_discovery: Mutex::new(plugin_discovery),
            nodes,
            active_routing_profile: Mutex::new(persisted.active_routing_profile.clone()),
            completed_playback_device_operations: Mutex::new(
                persisted.completed_playback_device_operations.clone(),
            ),
            codex_config: codex_config::CodexSession::default(),
            speech_cli: speech_cli::SpeechSession::default(),
            input_monitor: windows::input_monitor::InputMonitorManager::default(),
            audition_render: Mutex::new(None),
            voice_models: Mutex::new(None),
            meter_monitor: Arc::new(windows::meter_monitor::MeterMonitor::new()),
            pending_events: Mutex::new(Vec::new()),
            runtime_handle: Mutex::new(None),
            shortcut_dispatch_handle: Mutex::new(None),
        })
    }

    pub fn push_event(&self, event: BackendEvent) {
        self.pending_events.lock().push(event);
    }

    pub fn take_pending_events(&self) -> Vec<BackendEvent> {
        std::mem::take(&mut *self.pending_events.lock())
    }

    pub fn refresh_external_plugins(&self) -> Result<PluginDiscoveryReport> {
        let settings = self.config.load()?.settings;
        let search_paths: Vec<_> = settings
            .plugin_search_paths
            .iter()
            .map(|path| self.config.resolve_relative(path))
            .collect();
        let report = discover_process_plugins(&self.plugins, &search_paths);
        *self.plugin_discovery.lock() = report.clone();
        Ok(report)
    }

    pub fn plugin_discovery_report(&self) -> PluginDiscoveryReport {
        self.plugin_discovery.lock().clone()
    }

    pub fn start_runtime(&self) -> Result<()> {
        let mut guard = self.runtime_handle.lock();
        if guard.is_none() {
            *guard = Some(self.runtime.start()?);
        }
        Ok(())
    }

    pub fn stop_runtime(&self) {
        self.runtime_handle.lock().take();
    }

    pub fn start_shortcut_dispatcher(self: &Arc<Self>) -> Result<()> {
        let mut handle = self.shortcut_dispatch_handle.lock();
        if handle.is_none() {
            *handle = Some(spawn_dispatcher(
                Arc::downgrade(self),
                self.shortcut_engine.trigger_receiver(),
            )?);
        }
        Ok(())
    }

    /// Start the realtime thread and every enabled graph node.
    ///
    /// Both layers are idempotent: the runtime handle is created once and
    /// already-active node resources are reused by `NodeRuntimeManager`.
    pub fn start_graph_runtime(&self) -> Result<Vec<NodeStartResult>> {
        self.event_engine.start()?;
        if let Err(error) = self.action_engine.start() {
            self.event_engine.stop();
            return Err(error);
        }
        if let Err(error) = self.start_runtime() {
            self.action_engine.stop();
            self.event_engine.stop();
            return Err(error);
        }
        Ok(self.nodes.start_graph())
    }

    /// Stop graph-owned resources before stopping the realtime thread.
    pub fn stop_graph_runtime(&self) {
        self.nodes.stop_graph();
        self.stop_runtime();
        self.audition_render.lock().take();
        self.runtime.set_audition(None);
        self.action_engine.stop();
        self.event_engine.stop();
    }
}
