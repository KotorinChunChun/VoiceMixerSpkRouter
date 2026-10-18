//! 保存済み設定を変更せずに実行する、UI 非依存のセッション。
use crate::{
    actions::model::{ActionExecutionResult, ActionRuntimeEvent},
    audio::graph::{NodeId, NodeProperties, RuntimeState},
    config::{
        model::PersistedConfiguration, persistence::ConfigStore, routing_profile::RoutingProfile,
    },
    events::BackendEventMonitor,
    recording::state::RecordingOutput,
    Backend, BackendError, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandaloneNodeDiagnostics {
    pub node_id: NodeId,
    pub display_name: String,
    pub state: RuntimeState,
    pub runtime: crate::runtime_manager::NodeRuntimeDiagnostics,
}

/// Snapshot for a completed or still-running standalone session. It reports
/// observed work only; an empty result is not converted into a success claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandaloneDiagnostics {
    pub action_attempts: u64,
    pub action_completed: u64,
    pub action_succeeded: u64,
    pub action_pending: u64,
    pub action_failures: Vec<ActionExecutionResult>,
    pub outputs: Vec<RecordingOutput>,
    pub nodes: Vec<StandaloneNodeDiagnostics>,
}

pub struct StandaloneSession {
    backend: Arc<Backend>,
    name: String,
    directory: PathBuf,
    enabled: bool,
    output_directory: PathBuf,
    files: Vec<NodeId>,
    recorders: Vec<NodeId>,
    monitor: BackendEventMonitor,
    last_external: Instant,
    last_devices: Instant,
    action_attempts: u64,
    action_completed: u64,
    action_succeeded: u64,
    action_pending: u64,
    action_failures: Vec<ActionExecutionResult>,
    last_outputs: Vec<RecordingOutput>,
    preserve_directory: bool,
    last_diagnostics: Option<StandaloneDiagnostics>,
}

impl StandaloneSession {
    pub fn load(path: &Path, profile: Option<&str>) -> Result<Self> {
        let source = fs::canonicalize(path)?;
        let source_base = source.parent().unwrap();
        let directory =
            std::env::temp_dir().join(format!("voicemixer-standalone-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory)?;
        let result = Self::load_inner(&source, source_base, profile, directory.clone());
        if result.is_err() {
            let _ = fs::remove_dir_all(&directory);
        }
        result
    }
    fn load_inner(
        source: &Path,
        source_base: &Path,
        profile: Option<&str>,
        directory: PathBuf,
    ) -> Result<Self> {
        let bytes = fs::read(source)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let store = ConfigStore::new(directory.join("voicemixer.json"));
        let direct_profile = value.get("profile_id").is_some();
        if direct_profile && profile.is_some() {
            return Err(BackendError::InvalidOperation(
                "プロファイル JSON への --profile 指定はできません".into(),
            ));
        }
        let base = if direct_profile
            && source_base
                .file_name()
                .is_some_and(|x| x == "routing-profiles")
        {
            source_base.parent().unwrap_or(source_base)
        } else {
            source_base
        };
        let mut config: PersistedConfiguration = if direct_profile {
            let companion = base.join("voicemixer.json");
            if companion.exists() {
                serde_json::from_slice(&fs::read(companion)?)?
            } else {
                PersistedConfiguration::default()
            }
        } else {
            serde_json::from_slice(&bytes)?
        };
        let selected: Option<RoutingProfile> = if direct_profile {
            Some(serde_json::from_value(value)?)
        } else if let Some(id) = profile {
            let mut found = None;
            for entry in fs::read_dir(base.join("routing-profiles"))? {
                let path = entry?.path();
                if path.extension().is_some_and(|x| x == "json") {
                    let candidate: RoutingProfile = serde_json::from_slice(&fs::read(path)?)?;
                    if candidate.profile_id == id {
                        found = Some(candidate);
                        break;
                    }
                }
            }
            Some(found.ok_or_else(|| {
                BackendError::InvalidOperation(format!("プロファイルが見つかりません: {id}"))
            })?)
        } else {
            None
        };
        let current_name = config.active_routing_profile.as_ref().map(|id| {
            fs::read_dir(base.join("routing-profiles"))
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| fs::read(entry.path()).ok())
                .filter_map(|bytes| serde_json::from_slice::<RoutingProfile>(&bytes).ok())
                .find(|p| &p.profile_id == id)
                .map(|p| p.display_name)
                .unwrap_or_else(|| id.clone())
        });
        let name = if let Some(p) = selected {
            if !matches!(p.version, 1 | 2) {
                return Err(BackendError::InvalidOperation(format!(
                    "未対応プロファイル形式: {}",
                    p.version
                )));
            }
            config.nodes = p.nodes;
            config.routing_edges = p.routing_edges;
            config.event_edges = p.event_edges;
            config.node_layouts = p.node_layouts;
            config.registered_nodes = p.registered_nodes;
            config.obs_connections = p.obs_connections;
            config.shortcuts = p.shortcuts;
            config.replay_cache_metadata = p.replay_cache_metadata;
            p.display_name
        } else {
            current_name.unwrap_or_else(|| {
                source
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
        };
        for props in config.nodes.values_mut().map(|n| &mut n.properties).chain(
            config
                .registered_nodes
                .values_mut()
                .map(|n| &mut n.properties),
        ) {
            resolve_properties(props, base);
        }
        for path in &mut config.settings.plugin_search_paths {
            absolute(path, base);
        }
        for item in config.replay_cache_metadata.items.values_mut() {
            absolute(&mut item.path, base);
            let copied = directory
                .join(format!("cache-input-{}", item.item_id))
                .with_extension(item.path.extension().unwrap_or_default());
            fs::copy(&item.path, &copied)?;
            item.path = copied;
        }
        store.save(&config)?;
        // 互換移行は専用コピー上でのみ行う。
        let _ = store.load()?;
        let backend = Arc::new(Backend::new(store)?);
        let graph = backend.controller.snapshot();
        let active = || {
            graph
                .nodes
                .values()
                .filter(|n| n.enabled && n.runtime_enabled)
        };
        let files = active()
            .filter(|n| {
                matches!(
                    n.properties,
                    NodeProperties::FileInput { .. }
                        | NodeProperties::FolderReplay { .. }
                        | NodeProperties::MemoryReplay { .. }
                )
            })
            .map(|n| n.id)
            .collect();
        let recorders = active()
            .filter(|n| {
                matches!(
                    n.properties,
                    NodeProperties::Recorder { .. } | NodeProperties::ReplayCache { .. }
                )
            })
            .map(|n| n.id)
            .collect();
        let output_directory = base
            .join("standalone-recordings")
            .join(uuid::Uuid::new_v4().to_string());
        Ok(Self {
            backend,
            name,
            directory,
            output_directory,
            enabled: false,
            files,
            recorders,
            monitor: BackendEventMonitor::default(),
            last_external: Instant::now(),
            last_devices: Instant::now(),
            action_attempts: 0,
            action_completed: 0,
            action_succeeded: 0,
            action_pending: 0,
            action_failures: Vec::new(),
            last_outputs: Vec::new(),
            preserve_directory: false,
            last_diagnostics: None,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn profile_name(&self) -> &str {
        self.name()
    }
    pub fn backend(&self) -> &Arc<Backend> {
        &self.backend
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn has_file_inputs(&self) -> bool {
        !self.files.is_empty()
    }
    pub fn diagnostics(&self) -> StandaloneDiagnostics {
        self.last_diagnostics
            .clone()
            .unwrap_or_else(|| self.collect_diagnostics())
    }
    pub fn recorded_outputs(&self) -> &[RecordingOutput] {
        &self.last_outputs
    }
    fn collect_diagnostics(&self) -> StandaloneDiagnostics {
        let graph = self.backend.controller.snapshot();
        let mut nodes: Vec<_> = graph
            .nodes
            .values()
            .map(|node| StandaloneNodeDiagnostics {
                node_id: node.id,
                display_name: node.display_name.clone(),
                state: node.state,
                runtime: self.backend.nodes.runtime_diagnostics(node.id),
            })
            .collect();
        nodes.sort_by_key(|node| node.node_id);
        StandaloneDiagnostics {
            action_attempts: self.action_attempts,
            action_completed: self.action_completed,
            action_succeeded: self.action_succeeded,
            action_pending: self.action_pending,
            action_failures: self.action_failures.clone(),
            outputs: self.last_outputs.clone(),
            nodes,
        }
    }
    pub fn start(&mut self) -> Result<()> {
        if self.enabled {
            return Ok(());
        }
        self.last_diagnostics = None;
        self.last_outputs.clear();
        let result = self.start_inner();
        if result.is_err() {
            // start_graphの一部だけが起動した場合も、cleanupで資源を
            // 解放する前の状態と失敗理由をRunnerへ残す。
            self.last_diagnostics = Some(self.collect_diagnostics());
            self.backend.shortcut_dispatch_handle.lock().take();
            self.backend.stop_graph_runtime();
        } else {
            self.enabled = true;
        }
        result
    }
    fn start_inner(&self) -> Result<()> {
        // 再有効化も独立した録音として扱い、前回の固定ファイルを上書きしない。
        let mut graph = self.backend.controller.snapshot();
        for node in graph.nodes.values_mut() {
            if let NodeProperties::Recorder {
                path,
                directory_mode: false,
                ..
            } = &mut node.properties
            {
                let extension = path.extension().unwrap_or_default().to_string_lossy();
                *path = path.with_file_name(format!(
                    "recording_{}.{}",
                    uuid::Uuid::new_v4(),
                    extension
                ));
            }
        }
        self.backend.controller.replace_graph(graph)?;
        // 音声クロック開始前に出力を準備し、入力先頭の欠落を防ぐ。
        for result in self.backend.nodes.start_graph() {
            if matches!(result.state, RuntimeState::Error) {
                return Err(BackendError::InvalidOperation(format!(
                    "ノード {} 起動失敗: {}",
                    result.node_id,
                    result.message.unwrap_or_default()
                )));
            }
        }
        for id in &self.recorders {
            self.backend.nodes.start_recorder(*id)?;
            self.backend
                .nodes
                .set_recorder_completed_output_collection(*id, true)?;
        }
        // Device workers must be ready before a finite file source starts.
        // Process and Edge sources are deliberately excluded: they can be idle
        // until their external producer appears.
        self.backend
            .nodes
            .wait_for_device_workers(Duration::from_secs(3))?;
        for id in &self.files {
            let graph = self.backend.controller.snapshot();
            if matches!(
                graph.nodes[id].properties,
                NodeProperties::MemoryReplay { .. }
            ) && self
                .backend
                .nodes
                .replay_state(*id)?
                .duration_seconds
                .unwrap_or(0.0)
                == 0.0
            {
                continue; // 未録音のメモリ再生はイベント/ショートカットによる開始を待つ。
            }
            self.backend.nodes.play_replay(*id)?;
        }
        for _ in self.backend.shortcut_engine.trigger_receiver().try_iter() {}
        self.backend.start_shortcut_dispatcher()?;
        self.backend.start_graph_runtime()?;
        Ok(())
    }
    pub fn all_files_finished(&self) -> Result<bool> {
        if !self.enabled || self.files.is_empty() {
            return Ok(false);
        }
        for id in &self.files {
            if !self.backend.nodes.replay_state(*id)?.eof {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn tick(&mut self) -> Result<()> {
        self.tick_with_runtime_events(|_| {})
    }
    /// 既存のエラー処理を維持したまま、消費する音声イベントを呼出元にも渡す。
    /// 別Receiverとの取り合いを避け、メーター等を非音声スレッドで観測する。
    pub fn tick_with_runtime_events(
        &mut self,
        mut observe: impl FnMut(&crate::audio::engine::RuntimeEvent),
    ) -> Result<()> {
        self.tick_with_runtime_event_policy(|event| {
            observe(event);
            false
        })
    }
    /// trueを返した音声エラーは呼出元が復旧を担当する。それ以外の異常は従来どおり返す。
    /// 現在キューにあるイベントを最後まで観測し、過去の障害を再試行のたびに残さない。
    pub fn tick_with_runtime_event_policy(
        &mut self,
        observe: impl FnMut(&crate::audio::engine::RuntimeEvent) -> bool,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        // デバイスやノードの検査が失敗しても、音声イベントの消費は継続する。
        let runtime_result = drain_audio_events(&self.backend.runtime.event_receiver(), observe);
        if self.last_external.elapsed() >= Duration::from_millis(500) {
            self.backend.nodes.refresh_external_states();
            self.last_external = Instant::now();
        }
        if self.last_devices.elapsed() >= Duration::from_secs(2) {
            crate::commands::tauri_commands::refresh_devices(&self.backend)?;
            for result in self.backend.nodes.refresh_default_endpoint_bindings() {
                if matches!(result.state, RuntimeState::Error) {
                    return Err(BackendError::InvalidOperation(format!(
                        "既定デバイス変更後の再接続失敗: {}",
                        result.message.unwrap_or_default()
                    )));
                }
            }
            self.last_devices = Instant::now();
        }
        self.monitor.poll(&self.backend);
        for id in &self.recorders {
            self.last_outputs
                .extend(self.backend.nodes.drain_recorder_completed_outputs(*id)?);
        }
        for node in self.backend.controller.snapshot().nodes.values() {
            if node.enabled && node.runtime_enabled && node.state == RuntimeState::Error {
                return Err(BackendError::InvalidOperation(format!(
                    "ノード実行エラー: {} ({})",
                    node.display_name, node.id
                )));
            }
        }
        for _ in self.backend.event_engine.event_receiver().try_iter() {}
        self.drain_action_events()?;
        for _ in self.backend.shortcut_engine.event_receiver().try_iter() {}
        for id in &self.recorders {
            let state = self.backend.nodes.recorder_state(*id)?;
            if let Some(error) = state.error {
                return Err(BackendError::InvalidOperation(error));
            }
        }
        runtime_result
    }
    fn drain_runtime_events(
        &self,
        mut observe: impl FnMut(&crate::audio::engine::RuntimeEvent),
    ) -> Result<()> {
        drain_audio_events(&self.backend.runtime.event_receiver(), |event| {
            observe(&event);
            false
        })
    }
    fn drain_action_events(&mut self) -> Result<()> {
        let mut messages = Vec::new();
        for event in self.backend.action_engine.event_receiver().try_iter() {
            match event {
                ActionRuntimeEvent::ExecutionStarted { .. } => {
                    self.action_attempts = self.action_attempts.saturating_add(1);
                    self.action_pending = self.action_pending.saturating_add(1);
                }
                ActionRuntimeEvent::ExecutionFinished { result } => {
                    self.action_completed = self.action_completed.saturating_add(1);
                    self.action_pending = self.action_pending.saturating_sub(1);
                    if result.success {
                        self.action_succeeded = self.action_succeeded.saturating_add(1);
                    } else {
                        let message = result
                            .message
                            .clone()
                            .unwrap_or_else(|| "詳細のないアクション失敗".into());
                        if self.action_failures.len() == 64 {
                            self.action_failures.remove(0);
                        }
                        self.action_failures.push(result);
                        messages.push(message);
                    }
                }
                ActionRuntimeEvent::ConnectionStateChanged { .. } => {}
            }
        }
        if messages.is_empty() {
            Ok(())
        } else {
            Err(BackendError::InvalidOperation(format!(
                "アクション実行失敗: {}",
                messages.join(" | ")
            )))
        }
    }
    pub fn poll(&mut self) -> Result<()> {
        self.tick()
    }
    pub fn stop(&mut self) -> Result<Vec<RecordingOutput>> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        // クロック停止後に録音キューをフラッシュする。
        self.backend.shortcut_dispatch_handle.lock().take();
        self.backend.stop_runtime();
        let mut outputs = Vec::new();
        let mut failure = self.drain_runtime_events(|_| {}).err();
        for id in &self.recorders {
            match self.backend.nodes.stop_recorder(*id) {
                Ok(Some(output)) => outputs.push(output),
                Ok(None) => match self.backend.nodes.recorder_state(*id) {
                    Ok(state) => {
                        if let Some(output) = state.last_output {
                            outputs.push(output);
                        }
                    }
                    Err(error) => {
                        if failure.is_none() {
                            failure = Some(error);
                        }
                    }
                },
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
            match self.backend.nodes.drain_recorder_completed_outputs(*id) {
                Ok(completed) => outputs.extend(completed),
                Err(error) if failure.is_none() => failure = Some(error),
                Err(_) => {}
            }
        }
        // Joining action workers before collecting diagnostics preserves results
        // that complete during shutdown, while node handles are still alive.
        self.backend.action_engine.stop();
        if failure.is_none() {
            failure = self.drain_action_events().err();
        } else {
            let _ = self.drain_action_events();
        }
        let mut diagnostics = self.collect_diagnostics();
        self.backend.stop_graph_runtime();
        self.enabled = false;
        outputs.extend(self.last_outputs.iter().cloned());
        let mut seen = HashSet::new();
        outputs.retain(|output| seen.insert(output.path.clone()));
        for output in &mut outputs {
            if output.path.starts_with(&self.directory) {
                match (|| -> Result<()> {
                    fs::create_dir_all(&self.output_directory)?;
                    let destination = self.output_directory.join(output.path.file_name().unwrap());
                    fs::copy(&output.path, &destination)?;
                    output.path = destination;
                    Ok(())
                })() {
                    Ok(()) => {}
                    Err(error) => {
                        // 報告に残す元録音をDropで消さない。コピー先へ
                        // 退避できない失敗そのものは呼出し元へ返す。
                        self.preserve_directory = true;
                        if failure.is_none() {
                            failure = Some(error);
                        }
                    }
                }
            }
        }
        self.last_outputs = outputs.clone();
        diagnostics.outputs = outputs.clone();
        self.last_diagnostics = Some(diagnostics);
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(outputs)
        }
    }
}
impl Drop for StandaloneSession {
    fn drop(&mut self) {
        let _ = self.stop();
        if !self.preserve_directory {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}
fn absolute(path: &mut PathBuf, base: &Path) {
    if path.is_relative() && !path.as_os_str().is_empty() {
        *path = base.join(&*path);
    }
}
fn resolve_properties(props: &mut NodeProperties, base: &Path) {
    match props {
        NodeProperties::FileInput { path, .. } => absolute(path, base),
        NodeProperties::Recorder {
            path,
            directory_mode,
            ..
        } => {
            absolute(path, base);
            if !*directory_mode {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let extension = path.extension().unwrap_or_default().to_string_lossy();
                *path = path.with_file_name(format!("{stem}_{}.{extension}", uuid::Uuid::new_v4()));
            }
        }
        NodeProperties::LogOutput { config } => absolute(&mut config.path, base),
        NodeProperties::FolderReplay {
            path,
            selected_file,
            ..
        } => {
            absolute(path, base);
            if let Some(file) = selected_file {
                absolute(file, base);
            }
        }
        // Plugin パラメーターの相対パスは Plugin 自身のディレクトリ基準。
        _ => {}
    }
}

/// 生産中のキューを無期限に追わず、呼出時点の件数だけを一括消費する。
fn drain_audio_events(
    receiver: &crossbeam_channel::Receiver<crate::audio::engine::RuntimeEvent>,
    mut observe: impl FnMut(&crate::audio::engine::RuntimeEvent) -> bool,
) -> Result<()> {
    let mut failure = None;
    for event in receiver.try_iter().take(receiver.len()) {
        let handled = observe(&event);
        if let crate::audio::engine::RuntimeEvent::NodeState {
            node_id,
            state: RuntimeState::Error,
            message,
        } = event
        {
            if !handled && failure.is_none() {
                failure = Some(BackendError::InvalidOperation(format!(
                    "音声処理エラー {}: {}",
                    node_id,
                    message.unwrap_or_default()
                )));
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_error_drain_consumes_backlog_and_keeps_unhandled_failures() {
        use crate::audio::engine::RuntimeEvent;
        let (sender, receiver) = crossbeam_channel::bounded(256);
        let rvc = NodeId::new();
        let mic = NodeId::new();
        for _ in 0..100 {
            sender
                .send(RuntimeEvent::NodeState {
                    node_id: rvc,
                    state: RuntimeState::Error,
                    message: Some("過去のRVCエラー".into()),
                })
                .unwrap();
        }
        sender
            .send(RuntimeEvent::NodeState {
                node_id: mic,
                state: RuntimeState::Error,
                message: Some("現在のマイク異常".into()),
            })
            .unwrap();
        sender
            .send(RuntimeEvent::NodeState {
                node_id: rvc,
                state: RuntimeState::Running,
                message: None,
            })
            .unwrap();
        let mut seen = 0;
        let error = drain_audio_events(&receiver, |event| {
            seen += 1;
            matches!(event, RuntimeEvent::NodeState { node_id, .. } if *node_id == rvc)
        })
        .unwrap_err();
        assert_eq!(seen, 102);
        assert!(receiver.is_empty());
        assert!(error.to_string().contains("現在のマイク異常"));
        // 復旧後の次の巡回に、前回処理したエラーを持ち越さない。
        drain_audio_events(&receiver, |_| false).unwrap();
    }

    #[test]
    fn unhandled_audio_error_does_not_stop_remaining_observation() {
        use crate::audio::engine::RuntimeEvent;
        let (sender, receiver) = crossbeam_channel::bounded(4);
        for state in [
            RuntimeState::Error,
            RuntimeState::Error,
            RuntimeState::Running,
        ] {
            sender
                .send(RuntimeEvent::NodeState {
                    node_id: NodeId::new(),
                    state,
                    message: Some("観測対象".into()),
                })
                .unwrap();
        }
        let mut seen = 0;
        assert!(drain_audio_events(&receiver, |_| {
            seen += 1;
            false
        })
        .is_err());
        assert_eq!(seen, 3);
        assert!(receiver.is_empty());
    }

    #[test]
    fn runtime_observer_receives_live_meters_without_hiding_audio_errors() {
        use crate::audio::{
            engine::{AudioSource, RuntimeEvent},
            format::AudioBlock,
            graph::{NodeSpec, TestSignalConfig},
        };
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("meters.json");
        let props = NodeProperties::TestSignal {
            config: TestSignalConfig::default(),
            gain_db: 0.0,
            mute: false,
        };
        let source = NodeSpec::new(props.node_type(), "試験信号", props);
        let id = source.id;
        let mut config = PersistedConfiguration::default();
        config.nodes.insert(id, source);
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut session = StandaloneSession::load(&config_path, None).unwrap();
        session.start().unwrap();
        let mut peak = 0.0f32;
        let deadline = Instant::now() + Duration::from_secs(3);
        while peak == 0.0 && Instant::now() < deadline {
            session
                .tick_with_runtime_events(|event| {
                    if let RuntimeEvent::NodeMeter { node_id, meter } = event {
                        if *node_id == id {
                            peak = peak.max(meter.peak);
                        }
                    }
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!((peak - 10.0f32.powf(-18.0 / 20.0)).abs() < 0.001);
        struct FailingSource;
        impl AudioSource for FailingSource {
            fn read_into(&self, _: &mut AudioBlock) -> Result<()> {
                Err(BackendError::InvalidOperation("観測中の入力失敗".into()))
            }
        }
        session
            .backend
            .runtime
            .bind_source(id, Arc::new(FailingSource));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut observed_error = false;
        let mut failure = None;
        while failure.is_none() && Instant::now() < deadline {
            failure = session.tick_with_runtime_events(|event| {
                observed_error |= matches!(event, RuntimeEvent::NodeState { node_id, state: RuntimeState::Error, .. } if *node_id == id);
            }).err();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(observed_error);
        assert!(failure.unwrap().to_string().contains("観測中の入力失敗"));
        let _ = session.stop();
    }

    #[test]
    fn original_configuration_is_unchanged_and_session_paths_are_isolated() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("original.json");
        let config = PersistedConfiguration::default();
        let bytes = serde_json::to_vec(&config).unwrap();
        fs::write(&source, &bytes).unwrap();
        let first = StandaloneSession::load(&source, None).unwrap();
        let second = StandaloneSession::load(&source, None).unwrap();
        assert_ne!(first.backend.config.path(), second.backend.config.path());
        assert_eq!(first.name(), "original");
        assert!(!first.has_file_inputs());
        assert!(!first.all_files_finished().unwrap());
        drop(first);
        drop(second);
        assert_eq!(fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn absent_configuration_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        assert!(StandaloneSession::load(&root.path().join("absent.json"), None).is_err());
    }

    #[test]
    fn fixed_recordings_get_unique_paths_and_relative_input_uses_source_base() {
        let base = Path::new("C:/project");
        let mut input = NodeProperties::FileInput {
            path: "input.wav".into(),
            looped: false,
            loop_wait_ms: 0,
            gain_db: 0.0,
            mute: false,
        };
        resolve_properties(&mut input, base);
        if let NodeProperties::FileInput { path, .. } = input {
            assert_eq!(path, base.join("input.wav"));
        }
        let recorder = NodeProperties::Recorder {
            path: "output.wav".into(),
            format: Default::default(),
            directory_mode: false,
            auto_record: false,
            gain_db: 0.0,
            mute: false,
        };
        let mut first = recorder.clone();
        let mut second = recorder;
        resolve_properties(&mut first, base);
        resolve_properties(&mut second, base);
        if let (
            NodeProperties::Recorder { path: first, .. },
            NodeProperties::Recorder { path: second, .. },
        ) = (first, second)
        {
            assert_ne!(first, second);
            assert_eq!(first.parent(), Some(base));
        }
    }

    #[test]
    fn current_profile_name_is_used_without_replacing_current_graph() {
        let root = tempfile::tempdir().unwrap();
        let mut config = PersistedConfiguration::default();
        config.active_routing_profile = Some("my-profile".into());
        fs::write(
            root.path().join("voicemixer.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        fs::create_dir(root.path().join("routing-profiles")).unwrap();
        fs::write(
            root.path().join("routing-profiles/my-profile.json"),
            br#"{"version":2,"profile_id":"my-profile","display_name":"Saved Name"}"#,
        )
        .unwrap();
        let session = StandaloneSession::load(&root.path().join("voicemixer.json"), None).unwrap();
        assert_eq!(session.name(), "Saved Name");
        assert!(session.backend.controller.snapshot().nodes.is_empty());
    }

    #[test]
    fn stop_preserves_and_copies_prior_completed_take_once() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("session.json");
        fs::write(
            &config,
            serde_json::to_vec(&PersistedConfiguration::default()).unwrap(),
        )
        .unwrap();
        let mut session = StandaloneSession::load(&config, None).unwrap();
        session.start().unwrap();
        let source = session.directory.join("prior.wav");
        fs::write(&source, b"prior-take-pcm").unwrap();
        let output = RecordingOutput {
            path: source.clone(),
            cache_id: None,
            cache_item_id: None,
            created_unix_ms: 1,
            duration_seconds: 4.0,
        };
        session.last_outputs = vec![output.clone(), output];
        let outputs = session.stop().unwrap();
        assert_eq!(outputs.len(), 1);
        let preserved = outputs[0].path.clone();
        assert!(preserved.starts_with(&session.output_directory));
        assert_eq!(fs::read(&preserved).unwrap(), b"prior-take-pcm");
        drop(session);
        assert_eq!(fs::read(preserved).unwrap(), b"prior-take-pcm");
    }
}
