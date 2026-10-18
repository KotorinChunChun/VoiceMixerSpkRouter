use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    audio::graph::{
        EdgeId, GraphSnapshot, NodeId, NodeLayout, NodeProperties, NodeSpec, RecordingFormat,
        RegisteredNodeId, RuntimeState,
    },
    config::model::PersistedConfiguration,
    controller::GraphController,
    error::{BackendError, Result},
    registry::node_registry::{NodeRegistry, RegisteredNode, RegistrationOrigin},
    windows::devices::{AudioDeviceInfo, DeviceDirection},
    Backend,
};

pub const TEST_CONFIGURATION_PROFILE_NAME: &str = "現在環境 VB-CABLE / RVC E2E";
pub const TEST_CONFIGURATION_MARKER: &str = "[VoiceMixer 現在環境テスト構成]";
pub const TEST_RVC_PLUGIN_ID: &str = "voicemixer.rvc.passthrough";
pub const TEST_FIXTURE_SECONDS: usize = 10;

const FIXTURE_FILE_NAME: &str = "こんにちは。10秒間のマイクテストです.wav";
const FIXTURE_BYTES: &[u8] =
    include_bytes!("../../resources/audio/こんにちは。10秒間のマイクテストです.wav");
const SOURCE_NAME: &str = "テスト音声（10秒）";
const RVC_NAME: &str = "RVC Pass-through Mock";
const MULTI_INPUT_NAME: &str = "テスト Multi Input";
const VIRTUAL_DEVICE_NAME: &str = "VB-CABLE テスト出力";
const CAPTURE_NAME: &str = "VB-CABLE テストCapture";
const RECORDER_NAME: &str = "テスト録音";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct TestConfigurationProfileReport {
    pub profile_name: String,
    pub fixture_path: PathBuf,
    pub recording_directory: PathBuf,
    pub render_endpoint_id: String,
    pub render_endpoint_name: String,
    pub capture_endpoint_id: String,
    pub capture_endpoint_name: String,
    pub node_ids: BTreeMap<String, NodeId>,
    pub edge_ids: Vec<EdgeId>,
    pub revision: u64,
}

pub fn load_test_configuration_profile_with_devices(
    backend: &Backend,
    inputs: &[AudioDeviceInfo],
    outputs: &[AudioDeviceInfo],
) -> Result<TestConfigurationProfileReport> {
    // A profile replacement must release a previous settings-screen preview
    // before replacing the graph and its endpoint ownership.
    backend.input_monitor.stop_all();
    let render = select_vb_cable_device(
        outputs,
        DeviceDirection::Render,
        "CABLE (VB-Audio Virtual Cable)",
        "CABLE",
    )
    .ok_or_else(|| {
        BackendError::InvalidOperation(
            "VB-CABLEのRender Endpointが見つかりません。VB-CABLEの導入と有効状態を確認してください。"
                .into(),
        )
    })?;
    let capture = select_vb_cable_device(
        inputs,
        DeviceDirection::Capture,
        "CABLE Output (VB-Audio Virtual Cable)",
        "CABLE Output",
    )
    .ok_or_else(|| {
        BackendError::InvalidOperation(
            "VB-CABLEのCapture Endpointが見つかりません。VB-CABLEの導入と有効状態を確認してください。"
                .into(),
        )
    })?;

    // Fail before touching the current graph if the development mock package
    // was not installed or could not be discovered.
    backend.plugins.descriptor(TEST_RVC_PLUGIN_ID)?;

    let profile_directory = backend.config.base_dir().join("test-profile");
    let fixture_path = profile_directory.join(FIXTURE_FILE_NAME);
    let recording_directory = profile_directory.join("recordings");
    std::fs::create_dir_all(&recording_directory)?;
    write_test_fixture(&fixture_path)?;

    let previous_registry = backend.registry.snapshot();
    let mut next_registry = previous_registry.clone();

    let source_id = upsert_profile_definition(
        &mut next_registry,
        profile_definition(
            RegisteredNode::user(
                SOURCE_NAME,
                NodeProperties::FileInput {
                    path: fixture_path.clone(),
                    looped: false,
                    loop_wait_ms: 0,
                    gain_db: 0.0,
                    mute: false,
                },
            ),
            10,
        ),
    );

    let plugin_spec =
        backend
            .plugins
            .build_node_spec(TEST_RVC_PLUGIN_ID, RVC_NAME, BTreeMap::new())?;
    let mut plugin_definition = RegisteredNode::from_graph_node(&plugin_spec);
    plugin_definition.id = RegisteredNodeId::new();
    plugin_definition.origin = RegistrationOrigin::User;
    let plugin_id = upsert_profile_definition(
        &mut next_registry,
        profile_definition(plugin_definition, 20),
    );

    let multi_input_id = upsert_profile_definition(
        &mut next_registry,
        profile_definition(
            RegisteredNode::user(
                MULTI_INPUT_NAME,
                NodeProperties::MultiInput {
                    slots: Vec::new(),
                    legacy_ducking: None,
                    gain_db: 0.0,
                    mute: false,
                },
            ),
            30,
        ),
    );
    let mut virtual_device_definition = RegisteredNode::user(
        VIRTUAL_DEVICE_NAME,
        NodeProperties::Monitor {
            endpoint_id: render.endpoint_id.clone(),
            gain_db: 0.0,
            mute: false,
        },
    );
    virtual_device_definition.is_virtual = true;
    let virtual_device_id = upsert_profile_definition(
        &mut next_registry,
        profile_definition(virtual_device_definition, 40),
    );
    let capture_id = upsert_profile_definition(
        &mut next_registry,
        profile_definition(
            RegisteredNode::user(
                CAPTURE_NAME,
                NodeProperties::Microphone {
                    endpoint_id: capture.endpoint_id.clone(),
                    capture_method: crate::audio::graph::CaptureMethod::Auto,
                    gain_db: 0.0,
                    mute: false,
                },
            ),
            50,
        ),
    );
    let recorder_id = upsert_profile_definition(
        &mut next_registry,
        profile_definition(
            RegisteredNode::user(
                RECORDER_NAME,
                NodeProperties::Recorder {
                    path: recording_directory.clone(),
                    format: RecordingFormat::Wav,
                    directory_mode: true,
                    auto_record: false,
                    gain_db: 0.0,
                    mute: false,
                },
            ),
            60,
        ),
    );

    let staging_registry = NodeRegistry::from_nodes(next_registry.clone());
    let staging = GraphController::new();
    let source = place_node(&staging_registry, &staging, source_id, 60.0, 120.0)?;
    let plugin = place_node(&staging_registry, &staging, plugin_id, 340.0, 120.0)?;
    let multi_input = place_node(&staging_registry, &staging, multi_input_id, 620.0, 120.0)?;
    let virtual_device = place_node(&staging_registry, &staging, virtual_device_id, 900.0, 120.0)?;
    let capture_node = place_node(&staging_registry, &staging, capture_id, 60.0, 420.0)?;
    let recorder = place_node(&staging_registry, &staging, recorder_id, 900.0, 420.0)?;

    let edge_ids = vec![
        staging.connect_nodes_auto(source.id, plugin.id)?.id,
        staging.connect_nodes_auto(plugin.id, multi_input.id)?.id,
        staging
            .connect_nodes_auto(multi_input.id, virtual_device.id)?
            .id,
        staging.connect_nodes_auto(capture_node.id, recorder.id)?.id,
    ];
    let staged_graph = staging.snapshot();

    // Loading a configuration is intentionally a stopped-state operation. It
    // replaces graph placements, but keeps every unrelated registration.
    backend.nodes.stop_graph();
    backend.stop_runtime();
    let previous_graph = backend.controller.snapshot();
    let previous_persisted = backend.config.load()?;
    let next_persisted = persisted_with_graph(
        previous_persisted.clone(),
        &staged_graph,
        &next_registry,
        backend,
    );
    backend.config.save(&next_persisted)?;
    backend.registry.replace_all(next_registry);
    if let Err(error) = backend.controller.replace_graph(staged_graph) {
        backend.registry.replace_all(previous_registry);
        let _ = backend.controller.replace_graph(previous_graph);
        let _ = backend.config.save(&previous_persisted);
        return Err(error);
    }
    backend.nodes.refresh_recorder_inputs();

    let graph = backend.controller.snapshot();
    let node_ids = BTreeMap::from([
        ("source".into(), source.id),
        ("rvc".into(), plugin.id),
        ("multi_input".into(), multi_input.id),
        ("virtual_device".into(), virtual_device.id),
        ("capture".into(), capture_node.id),
        ("recorder".into(), recorder.id),
    ]);
    Ok(TestConfigurationProfileReport {
        profile_name: TEST_CONFIGURATION_PROFILE_NAME.into(),
        fixture_path,
        recording_directory,
        render_endpoint_id: render.endpoint_id.clone(),
        render_endpoint_name: render.friendly_name.clone(),
        capture_endpoint_id: capture.endpoint_id.clone(),
        capture_endpoint_name: capture.friendly_name.clone(),
        node_ids,
        edge_ids,
        revision: graph.revision,
    })
}

fn select_vb_cable_device<'a>(
    devices: &'a [AudioDeviceInfo],
    direction: DeviceDirection,
    exact_name: &str,
    preferred_prefix: &str,
) -> Option<&'a AudioDeviceInfo> {
    devices
        .iter()
        .filter(|device| device.direction == direction)
        .find(|device| device.friendly_name.eq_ignore_ascii_case(exact_name))
        .or_else(|| {
            devices
                .iter()
                .filter(|device| device.direction == direction)
                .find(|device| {
                    let name = device.friendly_name.to_ascii_lowercase();
                    name.starts_with(&preferred_prefix.to_ascii_lowercase())
                        && name.contains("vb-audio virtual cable")
                })
        })
        .or_else(|| {
            devices
                .iter()
                .filter(|device| device.direction == direction)
                .find(|device| {
                    device
                        .friendly_name
                        .to_ascii_lowercase()
                        .contains("vb-audio virtual cable")
                })
        })
}

fn profile_definition(mut definition: RegisteredNode, playback_order: i32) -> RegisteredNode {
    definition.enabled = true;
    definition.origin = RegistrationOrigin::User;
    definition.comment = TEST_CONFIGURATION_MARKER.into();
    definition.show_in_playback = true;
    definition.playback_order = playback_order;
    definition
}

fn upsert_profile_definition(
    registry: &mut HashMap<RegisteredNodeId, RegisteredNode>,
    mut desired: RegisteredNode,
) -> RegisteredNodeId {
    if let Some(existing_id) = registry.values().find_map(|existing| {
        (existing.comment == TEST_CONFIGURATION_MARKER
            && existing.node_type == desired.node_type
            && existing.display_name == desired.display_name)
            .then_some(existing.id)
    }) {
        desired.id = existing_id;
    }
    let id = desired.id;
    registry.insert(id, desired);
    id
}

fn place_node(
    registry: &NodeRegistry,
    controller: &GraphController,
    registered_node_id: RegisteredNodeId,
    x: f32,
    y: f32,
) -> Result<NodeSpec> {
    let node = registry.instantiate(registered_node_id, &controller.snapshot())?;
    let node = controller.create_node(node)?;
    controller.update_node_layout(NodeLayout::new(node.id, x, y))?;
    Ok(node)
}

fn persisted_with_graph(
    mut persisted: PersistedConfiguration,
    graph: &GraphSnapshot,
    registry: &HashMap<RegisteredNodeId, RegisteredNode>,
    backend: &Backend,
) -> PersistedConfiguration {
    persisted.nodes = graph
        .nodes
        .iter()
        .map(|(id, node)| {
            let mut node = node.clone();
            node.state = RuntimeState::Stopped;
            (*id, node)
        })
        .collect();
    persisted.routing_edges = graph.edges.clone();
    persisted.node_layouts = graph.layouts.clone();
    persisted.registered_nodes = registry.clone();
    persisted.device_history = backend.device_history.read().clone();
    persisted.replay_cache_metadata = backend.replay_caches.snapshot();
    persisted
}

fn write_test_fixture(path: &Path) -> Result<()> {
    if path.is_file() && std::fs::read(path)? == FIXTURE_BYTES {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, FIXTURE_BYTES)?;
    Ok(())
}
