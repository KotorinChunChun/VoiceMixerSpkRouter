use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    actions::model::{ObsConnectionConfig, ObsConnectionId},
    audio::graph::{
        ApplicationFilter, Edge, EndpointCaptureMode, EventEdge, GraphSnapshot, NodeId, NodeLayout,
        NodeProperties, NodeSpec, NodeType, PortId, RegisteredNodeId, SignalKind,
    },
    config::persistence::ConfigStore,
    controller::GraphController,
    error::{BackendError, Result},
    plugins::{
        builtins::{
            compressor::COMPRESSOR_PLUGIN_ID, ducking::DUCKING_PLUGIN_ID,
            equalizer::EQUALIZER_PLUGIN_ID, gate::GATE_PLUGIN_ID, limiter::LIMITER_PLUGIN_ID,
            noise_suppressor::NOISE_SUPPRESSOR_PLUGIN_ID,
        },
        create_default_plugin_manager,
    },
    registry::node_registry::RegisteredNode,
    replay::cache::ReplayCacheMetadata,
    routing::validator::{validate_graph, ValidationSeverity},
    shortcut::model::{ShortcutBinding, ShortcutId},
    windows::devices::{
        list_devices, AudioDeviceInfo, DeviceDirection, DEFAULT_COMMUNICATIONS_ENDPOINT,
        DEFAULT_CONSOLE_ENDPOINT,
    },
};

pub const ROUTING_PROFILE_DIRECTORY: &str = "routing-profiles";
pub const ROUTING_PROFILE_SCHEMA_VERSION: u32 = 2;
const LEGACY_ROUTING_PROFILE_SCHEMA_VERSION: u32 = 1;
const LEGACY_SAMPLE_EMPTY_PROFILE_ID: &str = "empty";
pub const SAMPLE_MONITOR_PROFILE_ID: &str = "microphone-monitor";
pub const SAMPLE_RECORDING_PROFILE_ID: &str = "file-recording";
pub const SAMPLE_BROADCAST_PROFILE_ID: &str = "broadcast-mic-chain";
pub const SAMPLE_DYNAMICS_PROFILE_ID: &str = "dynamics-adjustment";
pub const SAMPLE_BRIDGE_CAST_MIX_PROFILE_ID: &str = "bridge-cast-three-input-mix";
pub const SAMPLE_AUDIO_DIRECTORY: &str = "samples";
pub const SAMPLE_AUDIO_FILE_NAME: &str = "こんにちは。10秒間のマイクテストです.wav";
const SAMPLE_AUDIO_BYTES: &[u8] =
    include_bytes!("../../resources/audio/こんにちは。10秒間のマイクテストです.wav");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingProfile {
    pub version: u32,
    pub profile_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub nodes: HashMap<NodeId, NodeSpec>,
    #[serde(default)]
    pub routing_edges: Vec<Edge>,
    #[serde(default)]
    pub event_edges: Vec<EventEdge>,
    #[serde(default)]
    pub node_layouts: HashMap<NodeId, NodeLayout>,
    #[serde(default)]
    pub registered_nodes: HashMap<RegisteredNodeId, RegisteredNode>,
    #[serde(default)]
    pub obs_connections: HashMap<ObsConnectionId, ObsConnectionConfig>,
    #[serde(default)]
    pub shortcuts: HashMap<ShortcutId, ShortcutBinding>,
    #[serde(default)]
    pub replay_cache_metadata: ReplayCacheMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RoutingProfileSummary {
    pub profile_id: String,
    pub display_name: String,
    pub description: String,
    pub file_name: String,
}

impl RoutingProfile {
    pub fn graph(&self) -> GraphSnapshot {
        GraphSnapshot {
            revision: 0,
            nodes: self.nodes.clone(),
            edges: self.routing_edges.clone(),
            event_edges: self.event_edges.clone(),
            layouts: self.node_layouts.clone(),
        }
    }

    pub fn summary(&self, file_name: impl Into<String>) -> RoutingProfileSummary {
        RoutingProfileSummary {
            profile_id: self.profile_id.clone(),
            display_name: self.display_name.clone(),
            description: self.description.clone(),
            file_name: file_name.into(),
        }
    }
}

pub fn directory(config: &ConfigStore) -> PathBuf {
    config.base_dir().join(ROUTING_PROFILE_DIRECTORY)
}

pub fn profile_path(config: &ConfigStore, profile_id: &str) -> PathBuf {
    directory(config).join(format!("{profile_id}.json"))
}

pub fn ensure_samples(config: &ConfigStore) -> Result<()> {
    super::transaction::recover(&config.base_dir())?;
    let root = directory(config);
    fs::create_dir_all(&root)?;
    let legacy_empty = root.join(format!("{LEGACY_SAMPLE_EMPTY_PROFILE_ID}.json"));
    if legacy_empty.exists() {
        fs::remove_file(legacy_empty)?;
    }
    let sample_audio = sample_audio_path(config);
    if let Some(parent) = sample_audio.parent() {
        fs::create_dir_all(parent)?;
    }
    if !sample_audio.exists()
        || fs::metadata(&sample_audio)?.len() != SAMPLE_AUDIO_BYTES.len() as u64
    {
        fs::write(&sample_audio, SAMPLE_AUDIO_BYTES)?;
    }
    // Built-in sample projects are product data. Always refresh these known IDs
    // so an older generated sample cannot keep a stale path or graph shape.
    for profile in sample_profiles(config) {
        let path = root.join(format!("{}.json", profile.profile_id));
        write_profile(&path, &profile)?;
    }
    Ok(())
}

pub fn sample_audio_path(config: &ConfigStore) -> PathBuf {
    config
        .base_dir()
        .join(SAMPLE_AUDIO_DIRECTORY)
        .join(SAMPLE_AUDIO_FILE_NAME)
}

pub fn list(config: &ConfigStore) -> Result<Vec<RoutingProfileSummary>> {
    ensure_samples(config)?;
    let mut profiles = Vec::new();
    for entry in fs::read_dir(directory(config))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let profile = read_profile(&path)?;
        profiles.push(
            profile.summary(
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default(),
            ),
        );
    }
    profiles.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    Ok(profiles)
}

pub fn load(config: &ConfigStore, profile_id: &str) -> Result<RoutingProfile> {
    ensure_samples(config)?;
    for entry in fs::read_dir(directory(config))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let profile = read_profile(&path)?;
        if profile.profile_id == profile_id {
            return Ok(profile);
        }
    }
    Err(BackendError::InvalidOperation(format!(
        "ルーティング設定が見つかりません: {profile_id}"
    )))
}

pub fn save(config: &ConfigStore, profile: &RoutingProfile) -> Result<RoutingProfileSummary> {
    ensure_samples(config)?;
    if profile.display_name.trim().is_empty() {
        return Err(BackendError::InvalidOperation(
            "プロジェクト名を入力してください".into(),
        ));
    }
    let file_name = format!("{}.json", profile.profile_id);
    write_profile(&directory(config).join(&file_name), profile)?;
    Ok(profile.summary(file_name))
}

pub fn is_user_project(profile_id: &str) -> bool {
    profile_id
        .strip_prefix("project-")
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}

pub fn delete(config: &ConfigStore, profile_id: &str) -> Result<()> {
    super::transaction::recover(&config.base_dir())?;
    if !is_user_project(profile_id) {
        return Err(BackendError::InvalidOperation(
            "削除できるのはユーザープロジェクトだけです".into(),
        ));
    }
    // UUIDで検証したIDからのみパスを作り、サンプルや別ディレクトリを保護する。
    let path = directory(config).join(format!("{profile_id}.json"));
    let profile = read_profile(&path)?;
    if profile.profile_id != profile_id {
        return Err(BackendError::InvalidOperation(
            "プロジェクトIDが一致しません".into(),
        ));
    }
    fs::remove_file(path)?;
    Ok(())
}

fn write_profile(path: &Path, profile: &RoutingProfile) -> Result<()> {
    let mut profile = profile.clone();
    normalize_legacy_virtual_devices(&mut profile.nodes, &mut profile.registered_nodes);
    super::transaction::atomic_write(path, &serde_json::to_vec_pretty(&profile)?)
}

fn read_profile(path: &Path) -> Result<RoutingProfile> {
    let bytes = fs::read(path).map_err(|source| BackendError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut profile: RoutingProfile = serde_json::from_slice(&bytes)?;
    if !matches!(
        profile.version,
        LEGACY_ROUTING_PROFILE_SCHEMA_VERSION | ROUTING_PROFILE_SCHEMA_VERSION
    ) {
        return Err(BackendError::InvalidOperation(format!(
            "未対応のルーティング設定バージョンです: {}",
            profile.version
        )));
    }
    for node in profile.nodes.values_mut() {
        crate::plugins::migrate_legacy_rvc_plugin_id(&mut node.properties);
    }
    let migrated_virtual_devices =
        normalize_legacy_virtual_devices(&mut profile.nodes, &mut profile.registered_nodes);
    let migrated_ducking = has_legacy_multi_input_ducking(profile.nodes.values());
    migrate_legacy_multi_input_ducking(
        &profile.display_name,
        &mut profile.nodes,
        &mut profile.routing_edges,
        &profile.event_edges,
        &mut profile.node_layouts,
    )?;
    reject_unplaced_legacy_ducking(
        &profile.display_name,
        &mut profile.registered_nodes,
        &profile.nodes,
    )?;
    if migrated_ducking || migrated_virtual_devices {
        write_profile(path, &profile)?;
    }
    Ok(profile)
}

/// `virtual_device` was an output implementation detail. Preserve every
/// identifier and connection while making Monitor the one render-node shape.
pub(crate) fn normalize_legacy_virtual_devices(
    nodes: &mut HashMap<NodeId, NodeSpec>,
    registered: &mut HashMap<RegisteredNodeId, RegisteredNode>,
) -> bool {
    let mut changed = false;
    for node in nodes.values_mut() {
        if let NodeProperties::VirtualDevice {
            endpoint_id,
            gain_db,
            mute,
        } = node.properties.clone()
        {
            node.node_type = NodeType::Monitor;
            node.properties = NodeProperties::Monitor {
                endpoint_id,
                gain_db,
                mute,
            };
            if let Some(id) = node.registered_node_id {
                if let Some(definition) = registered.get_mut(&id) {
                    definition.is_virtual = true;
                } else {
                    let mut definition = RegisteredNode::from_graph_node(node);
                    definition.id = id;
                    definition.is_virtual = true;
                    registered.insert(id, definition);
                }
            } else {
                // 登録定義のない旧Graphも仮想属性を失わないよう補完する。Graph ID/Portsは維持する。
                let mut definition = RegisteredNode::from_graph_node(node);
                definition.is_virtual = true;
                node.registered_node_id = Some(definition.id);
                registered.insert(definition.id, definition);
            }
            changed = true;
        }
    }
    for definition in registered.values_mut() {
        if let NodeProperties::VirtualDevice {
            endpoint_id,
            gain_db,
            mute,
        } = definition.properties.clone()
        {
            definition.node_type = NodeType::Monitor;
            definition.is_virtual = true;
            definition.properties = NodeProperties::Monitor {
                endpoint_id,
                gain_db,
                mute,
            };
            changed = true;
        }
    }
    changed
}

pub(crate) fn has_legacy_multi_input_ducking<'a>(
    nodes: impl IntoIterator<Item = &'a NodeSpec>,
) -> bool {
    nodes.into_iter().any(|node| {
        matches!(
            &node.properties,
            NodeProperties::MultiInput {
                legacy_ducking: Some(_),
                ..
            }
        )
    })
}

/// A registration without a placed graph node has no source edges to map to
/// Main/Sidechain. Refusing it is safer than silently dropping a setting that
/// would become audible when the definition is later placed.
pub(crate) fn reject_unplaced_legacy_ducking(
    profile_name: &str,
    registered_nodes: &mut HashMap<
        RegisteredNodeId,
        crate::registry::node_registry::RegisteredNode,
    >,
    graph_nodes: &HashMap<NodeId, NodeSpec>,
) -> Result<()> {
    for (registered_id, node) in registered_nodes.iter_mut() {
        if matches!(
            &node.properties,
            NodeProperties::MultiInput {
                legacy_ducking: Some(_),
                ..
            }
        ) {
            if !graph_nodes
                .values()
                .any(|graph_node| graph_node.registered_node_id == Some(*registered_id))
            {
                return Err(BackendError::InvalidOperation(format!(
                    "Ducking移行に失敗しました（プロジェクト: {profile_name}、Multi Input: {}）: 配置されていない登録ノードはMain/Sidechainの経路を復元できません",
                    node.display_name
                )));
            }
            // The placed graph has already supplied the concrete ports and
            // edges used for conversion. A registered definition has no
            // independent executable route, so clear its legacy payload only
            // after confirming that it is linked to that placed graph.
            if let NodeProperties::MultiInput { legacy_ducking, .. } = &mut node.properties {
                *legacy_ducking = None;
            }
        }
    }
    Ok(())
}

/// Replaces the pre-v0.4.2 mixer-local ducking payload with a regular
/// Side-chain Ducking plugin graph. The caller only receives changes after
/// every legacy mixer has been converted, so malformed input cannot leave a
/// partially migrated graph in memory or on disk.
pub(crate) fn migrate_legacy_multi_input_ducking(
    profile_name: &str,
    nodes: &mut HashMap<NodeId, NodeSpec>,
    edges: &mut Vec<Edge>,
    event_edges: &[EventEdge],
    layouts: &mut HashMap<NodeId, NodeLayout>,
) -> Result<()> {
    let mut next_nodes = nodes.clone();
    let mut next_edges = edges.clone();
    let mut next_layouts = layouts.clone();
    let legacy: Vec<_> = next_nodes
        .values()
        .filter_map(|node| match &node.properties {
            NodeProperties::MultiInput {
                legacy_ducking: Some(config),
                ..
            } => Some((node.id, node.display_name.clone(), *config)),
            _ => None,
        })
        .collect();
    if legacy.is_empty() {
        return Ok(());
    }

    let manager = create_default_plugin_manager()?;
    for (mixer_id, mixer_name, config) in legacy {
        let failure = |reason: String| {
            BackendError::InvalidOperation(format!(
                "Ducking移行に失敗しました（プロジェクト: {profile_name}、Multi Input: {mixer_name}）: {reason}"
            ))
        };
        config.validate().map_err(|reason| failure(reason))?;
        let target_edge = next_edges
            .iter()
            .find(|edge| edge.to_node == mixer_id && edge.to_port == config.target_port_id)
            .cloned()
            .ok_or_else(|| failure("Target入力のEdgeがありません".into()))?;
        let trigger_edge = next_edges
            .iter()
            .find(|edge| edge.to_node == mixer_id && edge.to_port == config.trigger_port_id)
            .cloned()
            .ok_or_else(|| failure("Trigger入力のEdgeがありません".into()))?;
        let mixer = next_nodes
            .get(&mixer_id)
            .ok_or_else(|| failure("Multi Inputノードがありません".into()))?;
        let NodeProperties::MultiInput { slots, .. } = &mixer.properties else {
            return Err(failure("対象ノードがMulti Inputではありません".into()));
        };
        for (label, port_id, edge) in [
            ("Target", config.target_port_id, &target_edge),
            ("Trigger", config.trigger_port_id, &trigger_edge),
        ] {
            if !mixer
                .input_ports
                .iter()
                .any(|port| port.id == port_id && port.signal_kind == SignalKind::Audio)
            {
                return Err(failure(format!("{label}入力Portがありません")));
            }
            let source = next_nodes
                .get(&edge.from_node)
                .ok_or_else(|| failure(format!("{label}入力の接続元ノードがありません")))?;
            if !source
                .output_ports
                .iter()
                .any(|port| port.id == edge.from_port && port.signal_kind == SignalKind::Audio)
            {
                return Err(failure(format!(
                    "{label}入力の接続元Audio Portがありません"
                )));
            }
        }
        let controls_for = |port_id: PortId, edge: &Edge| {
            slots
                .iter()
                .find(|slot| slot.port_id == port_id)
                .map(|slot| (slot.gain_db, slot.mute))
                .unwrap_or((edge.gain_db, edge.mute))
        };
        if !slots
            .iter()
            .any(|slot| slot.port_id == config.target_port_id)
            || !slots
                .iter()
                .any(|slot| slot.port_id == config.trigger_port_id)
        {
            return Err(failure("対象入力のMix Slotがありません".into()));
        }
        let (target_gain, target_mute) = controls_for(config.target_port_id, &target_edge);
        let (trigger_gain, trigger_mute) = controls_for(config.trigger_port_id, &trigger_edge);
        let mut parameters = BTreeMap::new();
        parameters.insert("enabled".into(), serde_json::json!(config.enabled));
        parameters.insert(
            "threshold_db".into(),
            serde_json::json!(config.threshold_db),
        );
        parameters.insert(
            "reduction_db".into(),
            serde_json::json!(config.reduction_db),
        );
        parameters.insert("attack_ms".into(), serde_json::json!(config.attack_ms));
        parameters.insert("release_ms".into(), serde_json::json!(config.release_ms));
        let duck = manager.build_node_spec(
            DUCKING_PLUGIN_ID,
            format!("{} Ducking", mixer_name),
            parameters,
        )?;
        let main_port = plugin_port(&duck, "main", &failure)?;
        let sidechain_port = plugin_port(&duck, "sidechain", &failure)?;
        let output_port = duck
            .output_ports
            .iter()
            .find(|port| port.plugin_port_key.as_deref() == Some("audio_out"))
            .map(|port| port.id)
            .ok_or_else(|| failure("Ducking出力Portがありません".into()))?;
        let duck_id = duck.id;
        next_edges.retain(|edge| edge.id != target_edge.id);
        let mut main = Edge::new(
            target_edge.from_node,
            target_edge.from_port,
            duck_id,
            main_port,
        );
        main.gain_db = target_gain;
        main.mute = target_mute;
        let mut sidechain = Edge::new(
            trigger_edge.from_node,
            trigger_edge.from_port,
            duck_id,
            sidechain_port,
        );
        sidechain.gain_db = trigger_gain;
        sidechain.mute = trigger_mute;
        let output = Edge::new(duck_id, output_port, mixer_id, config.target_port_id);
        if let Some(NodeProperties::MultiInput {
            slots,
            legacy_ducking,
            ..
        }) = next_nodes
            .get_mut(&mixer_id)
            .map(|node| &mut node.properties)
        {
            let target_slot = slots
                .iter_mut()
                .find(|slot| slot.port_id == config.target_port_id)
                .expect("validated target slot");
            target_slot.edge_id = Some(output.id);
            target_slot.gain_db = 0.0;
            target_slot.mute = false;
            *legacy_ducking = None;
        }
        if let Some(layout) = next_layouts.get(&mixer_id).copied() {
            next_layouts.insert(
                duck_id,
                NodeLayout::new(duck_id, layout.x - 260.0, layout.y),
            );
        }
        next_nodes.insert(duck_id, duck);
        next_edges.extend([main, sidechain, output]);
    }
    let report = validate_graph(&GraphSnapshot {
        revision: 0,
        nodes: next_nodes.clone(),
        edges: next_edges.clone(),
        event_edges: event_edges.to_vec(),
        layouts: next_layouts.clone(),
    });
    if let Some(issue) = report.issues.iter().find(|issue| {
        issue.severity == ValidationSeverity::Error
            && issue.code != "required_plugin_input_unconnected"
    }) {
        return Err(BackendError::InvalidOperation(format!(
            "Ducking移行に失敗しました（プロジェクト: {profile_name}）: 移行後Graphが不正です: {}",
            issue.message
        )));
    }
    *nodes = next_nodes;
    *edges = next_edges;
    *layouts = next_layouts;
    Ok(())
}

fn plugin_port(
    node: &NodeSpec,
    key: &str,
    failure: &impl Fn(String) -> BackendError,
) -> Result<PortId> {
    node.input_ports
        .iter()
        .find(|port| port.plugin_port_key.as_deref() == Some(key))
        .map(|port| port.id)
        .ok_or_else(|| failure(format!("Ducking {key}入力Portがありません")))
}

fn sample_profiles(config: &ConfigStore) -> Vec<RoutingProfile> {
    let mut profiles = vec![
        microphone_monitor_profile(),
        recording_monitor_profile(config),
        broadcast_mic_chain_profile(),
        dynamics_adjustment_profile(config),
    ];
    if let (Ok(render_devices), Ok(capture_devices)) = (
        list_devices(DeviceDirection::Render),
        list_devices(DeviceDirection::Capture),
    ) {
        if let Some(profile) = bridge_cast_mix_profile(&render_devices, &capture_devices) {
            profiles.push(profile);
        }
    }
    profiles
}

fn find_endpoint(devices: &[AudioDeviceInfo], friendly_name: &str) -> Option<String> {
    devices
        .iter()
        .find(|device| {
            device.state.eq_ignore_ascii_case("active")
                && device.friendly_name.eq_ignore_ascii_case(friendly_name)
        })
        .map(|device| device.endpoint_id.clone())
}

fn bridge_cast_mix_profile(
    render_devices: &[AudioDeviceInfo],
    capture_devices: &[AudioDeviceInfo],
) -> Option<RoutingProfile> {
    let desktop_endpoint = find_endpoint(render_devices, "SYSTEM (BRIDGE CAST V2)")?;
    let chat_endpoint = find_endpoint(render_devices, "CHAT (BRIDGE CAST V2)")?;
    let mic_endpoint = find_endpoint(capture_devices, "MIC (BRIDGE CAST V2)")?;
    let monitor_endpoint = find_endpoint(render_devices, "スピーカー (BRIDGE CAST V2)")?;

    let desktop = NodeSpec::new(
        NodeType::EndpointLoopback,
        "デスクトップ再生音",
        NodeProperties::EndpointLoopback {
            endpoint_id: desktop_endpoint,
            filter: ApplicationFilter::default(),
            capture_mode: EndpointCaptureMode::EndpointLoopback,
            gain_db: 0.0,
            mute: false,
        },
    );
    let chat = NodeSpec::new(
        NodeType::EndpointLoopback,
        "BRIDGE CAST チャット",
        NodeProperties::EndpointLoopback {
            endpoint_id: chat_endpoint,
            filter: ApplicationFilter::default(),
            capture_mode: EndpointCaptureMode::EndpointLoopback,
            gain_db: 0.0,
            mute: false,
        },
    );
    let mic = NodeSpec::new(
        NodeType::Microphone,
        "BRIDGE CAST マイク発言",
        NodeProperties::Microphone {
            endpoint_id: mic_endpoint,
            capture_method: crate::audio::graph::CaptureMethod::Auto,
            gain_db: 0.0,
            mute: false,
        },
    );
    let mut mixer = NodeSpec::new(
        NodeType::MultiInput,
        "3入力統合ミックス",
        NodeProperties::MultiInput {
            slots: Vec::new(),
            legacy_ducking: None,
            gain_db: 0.0,
            mute: false,
        },
    );
    mixer.input_ports[0].name = "デスクトップ".into();
    mixer.add_multi_input_port("チャット")?;
    mixer.add_multi_input_port("マイク")?;
    let monitor = NodeSpec::new(
        NodeType::Monitor,
        "モニタリングデバイス",
        NodeProperties::Monitor {
            endpoint_id: monitor_endpoint,
            gain_db: 0.0,
            mute: false,
        },
    );
    let input_ids = [desktop.id, chat.id, mic.id];
    let mixer_id = mixer.id;
    let monitor_id = monitor.id;
    let staging = GraphController::new();
    for node in [desktop, chat, mic, mixer, monitor] {
        staging.create_node(node).expect("BRIDGE CAST sample node");
    }
    for input_id in input_ids {
        let edge = staging
            .connect_nodes_auto(input_id, mixer_id)
            .expect("BRIDGE CAST input route");
        staging
            .set_edge_controls(edge.id, -9.5, false)
            .expect("BRIDGE CAST input gain");
    }
    staging
        .connect_nodes_auto(mixer_id, monitor_id)
        .expect("BRIDGE CAST monitor route");

    let graph = staging.snapshot();
    let mut layouts = HashMap::new();
    for (index, id) in input_ids.iter().enumerate() {
        layouts.insert(*id, NodeLayout::new(*id, 80.0, 80.0 + index as f32 * 220.0));
    }
    layouts.insert(mixer_id, NodeLayout::new(mixer_id, 420.0, 300.0));
    layouts.insert(monitor_id, NodeLayout::new(monitor_id, 780.0, 180.0));
    Some(RoutingProfile {
        version: ROUTING_PROFILE_SCHEMA_VERSION,
        profile_id: SAMPLE_BRIDGE_CAST_MIX_PROFILE_ID.into(),
        display_name: "サンプル: BRIDGE CAST 3入力ミックス".into(),
        description: "デスクトップ再生音・BRIDGE CASTのチャット・マイク発言を統合し、実在するモニタリングデバイスへ出力します。仮想デバイスはデバイス一覧から必要な実在の端点を追加してください。".into(),
        nodes: graph.nodes,
        routing_edges: graph.edges,
        event_edges: graph.event_edges,
        node_layouts: layouts,
        registered_nodes: HashMap::new(),
        obs_connections: HashMap::new(),
        shortcuts: HashMap::new(),
        replay_cache_metadata: ReplayCacheMetadata::default(),
    })
}

fn microphone_monitor_profile() -> RoutingProfile {
    let default_input = NodeSpec::new(
        NodeType::Microphone,
        "既定の入力デバイス",
        NodeProperties::Microphone {
            endpoint_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            capture_method: crate::audio::graph::CaptureMethod::Auto,
            gain_db: 0.0,
            mute: false,
        },
    );
    let communication_input = NodeSpec::new(
        NodeType::Microphone,
        "既定の通信入力デバイス",
        NodeProperties::Microphone {
            endpoint_id: DEFAULT_COMMUNICATIONS_ENDPOINT.into(),
            capture_method: crate::audio::graph::CaptureMethod::Auto,
            gain_db: 0.0,
            mute: false,
        },
    );
    let default_output = NodeSpec::new(
        NodeType::Monitor,
        "既定の再生デバイス",
        NodeProperties::Monitor {
            endpoint_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            gain_db: 0.0,
            mute: false,
        },
    );
    let communication_output = NodeSpec::new(
        NodeType::Monitor,
        "既定の通信再生デバイス",
        NodeProperties::Monitor {
            endpoint_id: DEFAULT_COMMUNICATIONS_ENDPOINT.into(),
            gain_db: 0.0,
            mute: false,
        },
    );
    let default_input_id = default_input.id;
    let communication_input_id = communication_input.id;
    let default_output_id = default_output.id;
    let communication_output_id = communication_output.id;
    let staging = GraphController::new();
    for node in [
        default_input,
        communication_input,
        default_output,
        communication_output,
    ] {
        staging.create_node(node).expect("sample node is valid");
    }
    staging
        .connect_nodes_auto(default_input_id, default_output_id)
        .expect("default sample route is valid");
    staging
        .connect_nodes_auto(communication_input_id, communication_output_id)
        .expect("communications sample route is valid");
    let graph = staging.snapshot();
    let mut layouts = HashMap::new();
    layouts.insert(
        default_input_id,
        NodeLayout::new(default_input_id, 80.0, 120.0),
    );
    layouts.insert(
        default_output_id,
        NodeLayout::new(default_output_id, 460.0, 120.0),
    );
    layouts.insert(
        communication_input_id,
        NodeLayout::new(communication_input_id, 80.0, 360.0),
    );
    layouts.insert(
        communication_output_id,
        NodeLayout::new(communication_output_id, 460.0, 360.0),
    );
    RoutingProfile {
        version: ROUTING_PROFILE_SCHEMA_VERSION,
        profile_id: SAMPLE_MONITOR_PROFILE_ID.into(),
        display_name: "サンプル: マイク入力モニター".into(),
        description:
            "既定の入力・通信入力を、それぞれ既定の再生・通信再生へ送るサンプルプロジェクトです。"
                .into(),
        nodes: graph.nodes,
        routing_edges: graph.edges,
        event_edges: graph.event_edges,
        node_layouts: layouts,
        registered_nodes: HashMap::new(),
        obs_connections: HashMap::new(),
        shortcuts: HashMap::new(),
        replay_cache_metadata: ReplayCacheMetadata::default(),
    }
}

fn recording_monitor_profile(config: &ConfigStore) -> RoutingProfile {
    let source = NodeSpec::new(
        NodeType::FileInput,
        "サンプル音声ファイル",
        NodeProperties::FileInput {
            path: sample_audio_path(config),
            looped: false,
            loop_wait_ms: 0,
            gain_db: 0.0,
            mute: false,
        },
    );
    let recorder = NodeSpec::new(
        NodeType::Recorder,
        "サンプル録音",
        NodeProperties::Recorder {
            path: PathBuf::from("recordings"),
            format: crate::audio::graph::RecordingFormat::Wav,
            directory_mode: true,
            auto_record: false,
            gain_db: 0.0,
            mute: false,
        },
    );
    let monitor = NodeSpec::new(
        NodeType::Monitor,
        "既定の再生デバイス",
        NodeProperties::Monitor {
            endpoint_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            gain_db: 0.0,
            mute: false,
        },
    );
    let source_id = source.id;
    let recorder_id = recorder.id;
    let monitor_id = monitor.id;
    let staging = GraphController::new();
    for node in [source, recorder, monitor] {
        staging.create_node(node).expect("sample node is valid");
    }
    staging
        .connect_nodes_auto(source_id, recorder_id)
        .expect("sample recording route is valid");
    staging
        .connect_nodes_auto(source_id, monitor_id)
        .expect("sample monitoring route is valid");
    let graph = staging.snapshot();
    let mut layouts = HashMap::new();
    layouts.insert(source_id, NodeLayout::new(source_id, 80.0, 220.0));
    layouts.insert(recorder_id, NodeLayout::new(recorder_id, 460.0, 110.0));
    layouts.insert(monitor_id, NodeLayout::new(monitor_id, 460.0, 360.0));
    RoutingProfile {
        version: ROUTING_PROFILE_SCHEMA_VERSION,
        profile_id: SAMPLE_RECORDING_PROFILE_ID.into(),
        display_name: "サンプル: ファイル再生・録音".into(),
        description: "指定サンプル音声を録音しながら、既定の再生デバイスで確認します。".into(),
        nodes: graph.nodes,
        routing_edges: graph.edges,
        event_edges: graph.event_edges,
        node_layouts: layouts,
        registered_nodes: HashMap::new(),
        obs_connections: HashMap::new(),
        shortcuts: HashMap::new(),
        replay_cache_metadata: ReplayCacheMetadata::default(),
    }
}

fn broadcast_mic_chain_profile() -> RoutingProfile {
    let manager = create_default_plugin_manager().expect("built-in DSP plugins");
    let mic = NodeSpec::new(
        NodeType::Microphone,
        "既定の入力デバイス",
        NodeProperties::Microphone {
            endpoint_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            capture_method: crate::audio::graph::CaptureMethod::Auto,
            gain_db: 0.0,
            mute: false,
        },
    );
    let gate = manager
        .build_node_spec(GATE_PLUGIN_ID, "ゲート", Default::default())
        .expect("gate node");
    let nr = manager
        .build_node_spec(NOISE_SUPPRESSOR_PLUGIN_ID, "ノイズ抑制", Default::default())
        .expect("nr node");
    let eq = manager
        .build_node_spec(EQUALIZER_PLUGIN_ID, "EQ", Default::default())
        .expect("eq node");
    let comp = manager
        .build_node_spec(COMPRESSOR_PLUGIN_ID, "コンプレッサー", Default::default())
        .expect("compressor node");
    let limiter = manager
        .build_node_spec(LIMITER_PLUGIN_ID, "Limiter", Default::default())
        .expect("limiter node");
    let monitor = NodeSpec::new(
        NodeType::Monitor,
        "既定の再生デバイス",
        NodeProperties::Monitor {
            endpoint_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            gain_db: 0.0,
            mute: false,
        },
    );
    let ids = [
        mic.id, gate.id, nr.id, eq.id, comp.id, limiter.id, monitor.id,
    ];
    let staging = GraphController::new();
    for node in [mic, gate, nr, eq, comp, limiter, monitor] {
        staging.create_node(node).expect("broadcast sample node");
    }
    for pair in ids.windows(2) {
        staging
            .connect_nodes_auto(pair[0], pair[1])
            .expect("broadcast sample route");
    }
    let graph = staging.snapshot();
    let mut layouts = HashMap::new();
    for (index, id) in ids.iter().enumerate() {
        layouts.insert(
            *id,
            NodeLayout::new(*id, 80.0 + index as f32 * 200.0, 180.0),
        );
    }
    RoutingProfile {
        version: ROUTING_PROFILE_SCHEMA_VERSION,
        profile_id: SAMPLE_BROADCAST_PROFILE_ID.into(),
        display_name: "サンプル: 配信マイクチェーン".into(),
        description:
            "既定マイクをゲート→ノイズ抑制→EQ→コンプレッサー→Limiter→既定再生へ送るサンプルです。"
                .into(),
        nodes: graph.nodes,
        routing_edges: graph.edges,
        event_edges: graph.event_edges,
        node_layouts: layouts,
        registered_nodes: HashMap::new(),
        obs_connections: HashMap::new(),
        shortcuts: HashMap::new(),
        replay_cache_metadata: ReplayCacheMetadata::default(),
    }
}

/// 元プロジェクトの退避・復帰は呼出し側が担当する。生成直後は全ノード停止。
pub fn dynamics_adjustment_profile(config: &ConfigStore) -> RoutingProfile {
    use crate::audio::graph::{
        InputSelectorConfig, RecordingFormat, RuntimeState, TestSignalConfig,
    };
    let manager = create_default_plugin_manager().expect("built-in Dynamics plugins");
    let voice = NodeSpec::new(
        NodeType::FileInput,
        "原音: 10秒の声（ループ）",
        NodeProperties::FileInput {
            path: sample_audio_path(config),
            looped: true,
            loop_wait_ms: 1000,
            gain_db: 0.0,
            mute: false,
        },
    );
    let compressor = manager
        .build_node_spec(
            COMPRESSOR_PLUGIN_ID,
            "声: コンプレッサー処理後",
            BTreeMap::from([
                ("threshold_db".into(), serde_json::json!(-36.0)),
                ("ratio".into(), serde_json::json!(4.0)),
                ("makeup_db".into(), serde_json::json!(0.0)),
            ]),
        )
        .expect("compressor sample");
    let background = NodeSpec::new(
        NodeType::TestSignal,
        "原音: BGM代用の一定テスト音",
        NodeProperties::TestSignal {
            config: TestSignalConfig {
                level_db: -24.0,
                ..Default::default()
            },
            gain_db: 0.0,
            mute: false,
        },
    );
    let ducking = manager
        .build_node_spec(
            DUCKING_PLUGIN_ID,
            "テスト音: ダッキング処理後",
            BTreeMap::from([
                ("threshold_db".into(), serde_json::json!(-36.0)),
                ("reduction_db".into(), serde_json::json!(-12.0)),
            ]),
        )
        .expect("ducking sample");
    let selector = NodeSpec::new(
        NodeType::InputSelector,
        "A/B試聴: 声原音／圧縮後／BGM原音／抑制後",
        NodeProperties::InputSelector {
            config: InputSelectorConfig::default(),
            gain_db: 0.0,
            mute: false,
        },
    );
    let monitor = NodeSpec::new(
        NodeType::Monitor,
        "試聴: 既定の再生デバイス",
        NodeProperties::Monitor {
            endpoint_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            gain_db: 0.0,
            mute: false,
        },
    );
    let recorder = |name: &str, directory: &str| {
        NodeSpec::new(
            NodeType::Recorder,
            name,
            NodeProperties::Recorder {
                path: config
                    .base_dir()
                    .join("recordings")
                    .join("dynamics")
                    .join(directory),
                format: RecordingFormat::Wav,
                directory_mode: true,
                auto_record: false,
                gain_db: 0.0,
                mute: false,
            },
        )
    };
    let original_record = recorder("原音の比較録音（手動開始）", "original");
    let compressed_record = recorder("圧縮後の比較録音（手動開始）", "compressed");
    let ducked_record = recorder("抑制後の比較録音（手動開始）", "ducked");
    let staging = GraphController::new();
    for node in [
        &voice,
        &compressor,
        &background,
        &ducking,
        &selector,
        &monitor,
        &original_record,
        &compressed_record,
        &ducked_record,
    ] {
        staging
            .create_node(node.clone())
            .expect("Dynamics node valid");
    }
    staging
        .connect_nodes_auto(voice.id, compressor.id)
        .expect("voice compressor");
    for (from, key) in [(&background, "main"), (&voice, "sidechain")] {
        let port = ducking
            .input_ports
            .iter()
            .find(|port| port.plugin_port_key.as_deref() == Some(key))
            .unwrap();
        staging
            .connect_nodes(from.id, from.output_ports[0].id, ducking.id, port.id)
            .expect("ducking port route");
    }
    for from in [&voice, &compressor, &background, &ducking] {
        staging
            .connect_nodes_auto(from.id, selector.id)
            .expect("exclusive preview input");
    }
    staging
        .connect_nodes_auto(selector.id, monitor.id)
        .expect("exclusive monitoring route");
    for (source, destination) in [
        (&voice, &original_record),
        (&compressor, &compressed_record),
        (&ducking, &ducked_record),
    ] {
        staging
            .connect_nodes_auto(source.id, destination.id)
            .expect("comparison recording route");
    }
    let mut graph = staging.snapshot();
    let mut layouts = HashMap::new();
    for (node, x, y) in [
        (&voice, 80., 80.),
        (&background, 80., 400.),
        (&compressor, 400., 80.),
        (&ducking, 400., 400.),
        (&selector, 760., 240.),
        (&monitor, 1120., 240.),
        (&original_record, 760., -160.),
        (&compressed_record, 1120., -160.),
        (&ducked_record, 760., 560.),
    ] {
        layouts.insert(node.id, NodeLayout::new(node.id, x, y));
    }
    for node in graph.nodes.values_mut() {
        node.state = RuntimeState::Stopped;
        node.runtime_enabled = false;
    }
    RoutingProfile { version: ROUTING_PROFILE_SCHEMA_VERSION, profile_id: SAMPLE_DYNAMICS_PROFILE_ID.into(),
        display_name: "サンプル: Dynamics 調整用".into(),
        description: "10秒の声をループしてコンプレッサーとダッキングを調整します。A/B試聴の入力1=声原音、2=圧縮後、3=BGM代用テスト音、4=抑制後。一つだけ試聴し、録音は必要なノードで手動開始してください。音量一致補正ではありません。".into(),
        nodes:graph.nodes,routing_edges:graph.edges,event_edges:graph.event_edges,node_layouts:layouts,
        registered_nodes:HashMap::new(),obs_connections:HashMap::new(),shortcuts:HashMap::new(),replay_cache_metadata:ReplayCacheMetadata::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        audio::graph::{DuckingConfig, MixSlot, PluginInstanceId, TransformFailurePolicy},
        plugins::{LEGACY_RVC_KIKI_PLUGIN_ID, RVC_CUDA_PLUGIN_ID},
    };
    use std::collections::BTreeMap;

    #[test]
    fn legacy_mixer_ducking_becomes_a_sidechain_plugin_without_muting_trigger() {
        let source = NodeSpec::new(
            NodeType::TestSignal,
            "Trigger",
            NodeProperties::TestSignal {
                config: Default::default(),
                gain_db: 0.0,
                mute: false,
            },
        );
        let target = NodeSpec::new(
            NodeType::TestSignal,
            "Target",
            NodeProperties::TestSignal {
                config: Default::default(),
                gain_db: 0.0,
                mute: false,
            },
        );
        let mixer = NodeSpec::new(
            NodeType::MultiInput,
            "Legacy Mixer",
            NodeProperties::MultiInput {
                slots: Vec::new(),
                legacy_ducking: None,
                gain_db: 0.0,
                mute: false,
            },
        );
        let controller = GraphController::new();
        for node in [source.clone(), target.clone(), mixer.clone()] {
            controller.create_node(node).unwrap();
        }
        let trigger = controller.connect_nodes_auto(source.id, mixer.id).unwrap();
        let target_edge = controller.connect_nodes_auto(target.id, mixer.id).unwrap();
        let mut graph = controller.snapshot();
        let mut registered = RegisteredNode::from_graph_node(&graph.nodes[&mixer.id]);
        graph.nodes.get_mut(&mixer.id).unwrap().registered_node_id = Some(registered.id);
        let NodeProperties::MultiInput {
            slots,
            legacy_ducking,
            ..
        } = &mut graph.nodes.get_mut(&mixer.id).unwrap().properties
        else {
            panic!("expected mixer")
        };
        for slot in slots.iter_mut() {
            if slot.port_id == trigger.to_port {
                slot.gain_db = -3.0;
                slot.mute = true;
            }
            if slot.port_id == target_edge.to_port {
                slot.gain_db = -6.0;
            }
        }
        let config = DuckingConfig {
            enabled: false,
            trigger_port_id: trigger.to_port,
            target_port_id: target_edge.to_port,
            threshold_db: -30.0,
            reduction_db: -12.0,
            attack_ms: 25.0,
            release_ms: 250.0,
        };
        *legacy_ducking = Some(config);
        if let NodeProperties::MultiInput { legacy_ducking, .. } = &mut registered.properties {
            *legacy_ducking = Some(config);
        }
        let mut layouts = HashMap::from([(mixer.id, NodeLayout::new(mixer.id, 400.0, 50.0))]);
        // 実際の旧JSONを読み込み、登録定義を含めた保存と再読込を確認する。
        let persisted = crate::config::model::PersistedConfiguration {
            nodes: graph.nodes.clone(),
            routing_edges: graph.edges.clone(),
            node_layouts: layouts.clone(),
            registered_nodes: HashMap::from([(registered.id, registered.clone())]),
            ..Default::default()
        };
        let mut legacy_json = serde_json::to_value(&persisted).unwrap();
        legacy_json["nodes"][mixer.id.to_string()]["properties"]["ducking"] =
            serde_json::to_value(config).unwrap();
        legacy_json["registered_nodes"][registered.id.to_string()]["properties"]["ducking"] =
            serde_json::to_value(config).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(temp.path().join("legacy.json"));
        fs::write(store.path(), serde_json::to_vec(&legacy_json).unwrap()).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.nodes.len(), graph.nodes.len() + 1);
        let saved = fs::read(store.path()).unwrap();
        assert_eq!(store.load().unwrap().nodes.len(), loaded.nodes.len());
        assert_eq!(fs::read(store.path()).unwrap(), saved);
        // 実Portが欠落した旧設定では元のファイルを上書きしない。
        legacy_json["nodes"][mixer.id.to_string()]["input_ports"] = serde_json::json!([]);
        let broken = serde_json::to_vec(&legacy_json).unwrap();
        fs::write(store.path(), &broken).unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(store.path()).unwrap(), broken);
        migrate_legacy_multi_input_ducking(
            "Legacy Project",
            &mut graph.nodes,
            &mut graph.edges,
            &graph.event_edges,
            &mut layouts,
        )
        .unwrap();
        let mut registered_nodes = HashMap::from([(registered.id, registered)]);
        reject_unplaced_legacy_ducking("Legacy Project", &mut registered_nodes, &graph.nodes)
            .unwrap();

        let duck = graph
            .nodes
            .values()
            .find(|node| matches!(&node.properties, NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == DUCKING_PLUGIN_ID))
            .unwrap();
        assert!(
            graph.edges.iter().any(|edge| edge.id == trigger.id),
            "trigger remains audible in the mixer"
        );
        let main = graph
            .edges
            .iter()
            .find(|edge| edge.to_node == duck.id && edge.to_port == duck.input_ports[0].id)
            .unwrap();
        assert_eq!((main.gain_db, main.mute), (-6.0, false));
        let sidechain = graph
            .edges
            .iter()
            .find(|edge| edge.to_node == duck.id && edge.to_port == duck.input_ports[1].id)
            .unwrap();
        assert_eq!((sidechain.gain_db, sidechain.mute), (-3.0, true));
        assert!(layouts.contains_key(&duck.id));
        assert!(matches!(
            graph.nodes[&mixer.id].properties,
            NodeProperties::MultiInput {
                legacy_ducking: None,
                ..
            }
        ));
        assert!(matches!(
            registered_nodes.values().next().unwrap().properties,
            NodeProperties::MultiInput {
                legacy_ducking: None,
                ..
            }
        ));
        assert!(
            matches!(duck.properties, NodeProperties::TransformPlugin { ref parameters, .. } if parameters["enabled"] == serde_json::json!(false))
        );
    }

    #[test]
    fn malformed_legacy_ducking_leaves_the_graph_unchanged() {
        let mixer = NodeSpec::new(
            NodeType::MultiInput,
            "Broken Mixer",
            NodeProperties::MultiInput {
                slots: vec![MixSlot {
                    port_id: PortId::new(),
                    edge_id: None,
                    gain_db: 0.0,
                    mute: false,
                }],
                legacy_ducking: Some(DuckingConfig {
                    enabled: true,
                    trigger_port_id: PortId::new(),
                    target_port_id: PortId::new(),
                    threshold_db: -30.0,
                    reduction_db: -12.0,
                    attack_ms: 25.0,
                    release_ms: 250.0,
                }),
                gain_db: 0.0,
                mute: false,
            },
        );
        let mut nodes = HashMap::from([(mixer.id, mixer)]);
        let original = nodes.clone();
        let mut edges = Vec::new();
        let mut layouts = HashMap::new();
        let error =
            migrate_legacy_multi_input_ducking("Broken", &mut nodes, &mut edges, &[], &mut layouts)
                .unwrap_err();
        assert!(error.to_string().contains("Broken"));
        assert_eq!(nodes, original);
        assert!(edges.is_empty());
    }

    #[test]
    fn samples_are_created_and_can_be_loaded() {
        let temp = tempfile::tempdir().unwrap();
        let config = ConfigStore::new(temp.path().join("voicemixer.json"));

        let profiles = list(&config).unwrap();
        assert!((4..=5).contains(&profiles.len()));
        assert!(profiles
            .iter()
            .all(|profile| profile.display_name.starts_with("サンプル")));
        assert!(!directory(&config).join("empty.json").exists());
        let sample_audio = sample_audio_path(&config);
        assert!(sample_audio.is_file());
        assert_eq!(
            fs::metadata(&sample_audio).unwrap().len(),
            SAMPLE_AUDIO_BYTES.len() as u64
        );

        let monitor = load(&config, SAMPLE_MONITOR_PROFILE_ID).unwrap();
        assert_eq!(monitor.routing_edges.len(), 2);
        assert_eq!(monitor.nodes.len(), 4);
        assert_eq!(monitor.node_layouts.len(), 4);

        let recording = load(&config, SAMPLE_RECORDING_PROFILE_ID).unwrap();
        assert_eq!(recording.nodes.len(), 3);
        assert_eq!(recording.routing_edges.len(), 2);
        let file_input = recording
            .nodes
            .values()
            .find(|node| node.node_type == NodeType::FileInput)
            .unwrap();
        let NodeProperties::FileInput { path, .. } = &file_input.properties else {
            panic!("expected file input")
        };
        assert_eq!(path, &sample_audio);
        assert!(recording.nodes.values().any(|node| {
            matches!(
                &node.properties,
                NodeProperties::Monitor { endpoint_id, .. }
                    if endpoint_id == DEFAULT_CONSOLE_ENDPOINT
            )
        }));

        let chain = load(&config, SAMPLE_BROADCAST_PROFILE_ID).unwrap();
        assert_eq!(chain.nodes.len(), 7);
        assert_eq!(chain.routing_edges.len(), 6);
        assert!(chain.nodes.values().any(|node| {
            matches!(&node.properties, NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == GATE_PLUGIN_ID)
        }));
        assert!(chain.nodes.values().any(|node| {
            matches!(&node.properties, NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == NOISE_SUPPRESSOR_PLUGIN_ID)
        }));
        assert!(chain.nodes.values().any(|node| {
            matches!(&node.properties, NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == EQUALIZER_PLUGIN_ID)
        }));
        assert!(chain.nodes.values().any(|node| {
            matches!(&node.properties, NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == COMPRESSOR_PLUGIN_ID)
        }));
        assert!(chain.nodes.values().any(|node| {
            matches!(&node.properties, NodeProperties::TransformPlugin { plugin_id, .. } if plugin_id == LIMITER_PLUGIN_ID)
        }));
    }

    #[test]
    fn bridge_cast_devices_create_three_input_monitor_sample() {
        fn device(
            endpoint_id: &str,
            friendly_name: &str,
            direction: DeviceDirection,
        ) -> AudioDeviceInfo {
            AudioDeviceInfo {
                endpoint_id: endpoint_id.into(),
                friendly_name: friendly_name.into(),
                direction,
                state: "Active".into(),
                is_default: false,
                is_default_communications: false,
                current_sample_rate: Some(48_000),
                channels: Some(2),
                supported_sample_rates: vec![48_000],
                user_visible: true,
                user_sort_order: 0,
            }
        }

        let render = vec![
            device("system", "SYSTEM (BRIDGE CAST V2)", DeviceDirection::Render),
            device("chat", "CHAT (BRIDGE CAST V2)", DeviceDirection::Render),
            device(
                "monitor",
                "スピーカー (BRIDGE CAST V2)",
                DeviceDirection::Render,
            ),
        ];
        let capture = vec![device(
            "mic",
            "MIC (BRIDGE CAST V2)",
            DeviceDirection::Capture,
        )];

        let profile = bridge_cast_mix_profile(&render, &capture).unwrap();
        assert_eq!(profile.nodes.len(), 5);
        assert_eq!(profile.routing_edges.len(), 4);
        assert_eq!(profile.node_layouts.len(), 5);
        let mixer = profile
            .nodes
            .values()
            .find(|node| node.node_type == NodeType::MultiInput)
            .unwrap();
        assert_eq!(mixer.input_ports.len(), 3);
        let NodeProperties::MultiInput { slots, .. } = &mixer.properties else {
            panic!("expected multi input")
        };
        assert!(slots.iter().all(|slot| slot.gain_db == -9.5));
        assert_eq!(
            profile
                .nodes
                .values()
                .filter(|node| node.node_type == NodeType::Monitor)
                .count(),
            1
        );
    }

    #[test]
    fn load_migrates_legacy_rvc_plugin_id() {
        let temp = tempfile::tempdir().unwrap();
        let config = ConfigStore::new(temp.path().join("voicemixer.json"));
        let node = NodeSpec::new(
            NodeType::TransformPlugin,
            "Legacy RVC",
            NodeProperties::TransformPlugin {
                plugin_id: LEGACY_RVC_KIKI_PLUGIN_ID.into(),
                instance_id: PluginInstanceId::new(),
                parameters: BTreeMap::new(),
                bypass: false,
                failure_policy: TransformFailurePolicy::Silence,
                switch_policy: crate::audio::graph::SwitchPolicy::Silence,
                gain_db: 0.0,
                mute: false,
            },
        );
        let mut profile = RoutingProfile {
            version: ROUTING_PROFILE_SCHEMA_VERSION,
            profile_id: "legacy-rvc".into(),
            display_name: "Legacy RVC".into(),
            description: String::new(),
            nodes: HashMap::new(),
            routing_edges: Vec::new(),
            event_edges: Vec::new(),
            node_layouts: HashMap::new(),
            registered_nodes: HashMap::new(),
            obs_connections: HashMap::new(),
            shortcuts: HashMap::new(),
            replay_cache_metadata: ReplayCacheMetadata::default(),
        };
        profile.nodes.insert(node.id, node);
        fs::create_dir_all(directory(&config)).unwrap();
        write_profile(&directory(&config).join("legacy-rvc.json"), &profile).unwrap();

        let loaded = load(&config, "legacy-rvc").unwrap();
        let NodeProperties::TransformPlugin { plugin_id, .. } =
            &loaded.nodes.values().next().unwrap().properties
        else {
            panic!("expected transform plugin")
        };
        assert_eq!(plugin_id, RVC_CUDA_PLUGIN_ID);
    }

    #[test]
    fn version_one_profile_loads_with_empty_full_profile_fields() {
        let temp = tempfile::tempdir().unwrap();
        let config = ConfigStore::new(temp.path().join("voicemixer.json"));
        fs::create_dir_all(directory(&config)).unwrap();
        fs::write(
            directory(&config).join("legacy-v1.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "profile_id": "legacy-v1",
                "display_name": "Legacy v1",
                "nodes": {},
                "routing_edges": [],
                "event_edges": [],
                "node_layouts": {}
            }))
            .unwrap(),
        )
        .unwrap();

        let loaded = load(&config, "legacy-v1").unwrap();
        assert_eq!(loaded.version, LEGACY_ROUTING_PROFILE_SCHEMA_VERSION);
        assert!(loaded.registered_nodes.is_empty());
        assert!(loaded.obs_connections.is_empty());
        assert!(loaded.shortcuts.is_empty());
        assert!(loaded.replay_cache_metadata.caches.is_empty());
    }
}
