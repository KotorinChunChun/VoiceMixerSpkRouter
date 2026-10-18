use std::collections::HashMap;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::{
    audio::graph::{
        GraphSnapshot, GroupProcessorId, MemoryReplayMode, MixSlot, NodeId, NodeProperties,
        NodeSpec, NodeType, PluginInstanceId, PortId, PortSpec, RegisteredNodeId, RuntimeState,
        RvcInstanceId,
    },
    error::{BackendError, Result},
    registry::instance_policy::InstancePolicy,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationOrigin {
    AutoDetected,
    User,
    Migrated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct RegisteredNode {
    pub id: RegisteredNodeId,
    pub node_type: NodeType,
    pub display_name: String,
    pub enabled: bool,
    pub origin: RegistrationOrigin,
    /// Device classification kept outside the display name.
    #[serde(default)]
    pub is_virtual: bool,
    #[serde(default)]
    pub resource_key: Option<String>,
    pub properties: NodeProperties,
    #[serde(default)]
    pub input_ports: Vec<PortSpec>,
    #[serde(default)]
    pub output_ports: Vec<PortSpec>,
    #[serde(default)]
    pub comment: String,
    #[serde(default = "default_true")]
    pub show_in_playback: bool,
    #[serde(default)]
    pub playback_order: i32,
    #[serde(default)]
    pub instance_policy: InstancePolicy,
    #[serde(default)]
    pub presets: Vec<NodePreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct NodePreset {
    pub name: String,
    pub parameters: std::collections::BTreeMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

impl RegisteredNode {
    pub fn from_graph_node(node: &NodeSpec) -> Self {
        Self {
            id: node.registered_node_id.unwrap_or_default(),
            node_type: node.node_type,
            display_name: node.display_name.clone(),
            enabled: node.enabled,
            origin: RegistrationOrigin::Migrated,
            is_virtual: false,
            resource_key: resource_key(&node.properties),
            properties: node.properties.clone(),
            input_ports: node.input_ports.clone(),
            output_ports: node.output_ports.clone(),
            comment: node.comment.clone(),
            show_in_playback: node.show_in_playback,
            playback_order: node.playback_order,
            instance_policy: InstancePolicy::for_node_type(node.node_type),
            presets: Vec::new(),
        }
    }

    pub fn user(display_name: impl Into<String>, properties: NodeProperties) -> Self {
        let node_type = properties.node_type();
        let template = NodeSpec::new(node_type, display_name, properties);
        let mut out = Self::from_graph_node(&template);
        out.id = RegisteredNodeId::new();
        out.origin = RegistrationOrigin::User;
        out
    }

    pub(crate) fn instantiate(&self) -> NodeSpec {
        let input_ports = fresh_ports(&self.input_ports);
        let output_ports = fresh_ports(&self.output_ports);
        let port_map: HashMap<PortId, PortId> = self
            .input_ports
            .iter()
            .zip(&input_ports)
            .chain(self.output_ports.iter().zip(&output_ports))
            .map(|(old, new)| (old.id, new.id))
            .collect();
        let properties = match &self.properties {
            NodeProperties::TransformPlugin {
                plugin_id,
                parameters,
                bypass,
                failure_policy,
                switch_policy,
                gain_db,
                mute,
                ..
            } => NodeProperties::TransformPlugin {
                plugin_id: plugin_id.clone(),
                instance_id: PluginInstanceId::new(),
                parameters: parameters.clone(),
                bypass: *bypass,
                failure_policy: *failure_policy,
                switch_policy: *switch_policy,
                gain_db: *gain_db,
                mute: *mute,
            },
            NodeProperties::AudioProcessingGroup {
                flow,
                gain_db,
                mute,
            } => NodeProperties::AudioProcessingGroup {
                flow: flow
                    .iter()
                    .map(|member| {
                        let mut member = member.clone();
                        member.id = GroupProcessorId::new();
                        member.instance_id = PluginInstanceId::new();
                        member
                    })
                    .collect(),
                gain_db: *gain_db,
                mute: *mute,
            },
            NodeProperties::Rvc {
                bypass,
                auto_bypass_on_error,
                gain_db,
                mute,
                ..
            } => NodeProperties::Rvc {
                instance_id: RvcInstanceId::new(),
                bypass: *bypass,
                auto_bypass_on_error: *auto_bypass_on_error,
                gain_db: *gain_db,
                mute: *mute,
            },
            NodeProperties::MultiInput {
                slots,
                legacy_ducking: _,
                gain_db,
                mute,
            } => NodeProperties::MultiInput {
                slots: slots
                    .iter()
                    .filter_map(|slot| {
                        port_map.get(&slot.port_id).map(|port_id| MixSlot {
                            port_id: *port_id,
                            edge_id: None,
                            gain_db: slot.gain_db,
                            mute: slot.mute,
                        })
                    })
                    .collect(),
                legacy_ducking: None,
                gain_db: *gain_db,
                mute: *mute,
            },
            NodeProperties::InputSelector {
                config,
                gain_db,
                mute,
            } => NodeProperties::InputSelector {
                config: crate::audio::graph::InputSelectorConfig {
                    active_port_id: config
                        .active_port_id
                        .and_then(|port_id| port_map.get(&port_id).copied()),
                    default_port_id: config
                        .default_port_id
                        .and_then(|id| port_map.get(&id).copied()),
                    inputs: config
                        .inputs
                        .iter()
                        .filter_map(|input| {
                            port_map.get(&input.port_id).map(|id| {
                                crate::audio::graph::SwitcherInputConfig {
                                    port_id: *id,
                                    ..input.clone()
                                }
                            })
                        })
                        .collect(),
                    ..config.clone()
                },
                gain_db: *gain_db,
                mute: *mute,
            },
            NodeProperties::ChannelMapper {
                config,
                gain_db,
                mute,
            } => {
                let mut config = config.clone();
                config.port_map = config
                    .port_map
                    .iter()
                    .filter_map(|entry| {
                        port_map.get(&entry.source_port_id).map(|id| {
                            crate::audio::graph::ChannelPortMapEntry {
                                source_port_id: *id,
                                ..entry.clone()
                            }
                        })
                    })
                    .collect();
                NodeProperties::ChannelMapper {
                    config,
                    gain_db: *gain_db,
                    mute: *mute,
                }
            }
            other => other.clone(),
        };

        let mut node = if input_ports.is_empty() && output_ports.is_empty() {
            NodeSpec::new(self.node_type, self.display_name.clone(), properties)
        } else {
            NodeSpec {
                id: NodeId::new(),
                registered_node_id: Some(self.id),
                node_type: self.node_type,
                display_name: self.display_name.clone(),
                enabled: true,
                runtime_enabled: true,
                state: RuntimeState::Stopped,
                comment: self.comment.clone(),
                show_in_playback: self.show_in_playback,
                playback_order: self.playback_order,
                input_ports,
                output_ports,
                properties,
            }
        };
        node.registered_node_id = Some(self.id);
        node.enabled = true;
        node.runtime_enabled = true;
        node.state = RuntimeState::Stopped;
        node.comment = self.comment.clone();
        node.show_in_playback = self.show_in_playback;
        node.playback_order = self.playback_order;
        node
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn group_instantiation_rekeys_group_and_plugin_instances() {
        let registered = RegisteredNode::user(
            "group",
            NodeProperties::AudioProcessingGroup {
                flow: vec![crate::audio::graph::GroupProcessorSpec {
                    id: GroupProcessorId::new(),
                    display_name: "compressor".into(),
                    plugin_id: "audio-router.builtin.compressor".into(),
                    instance_id: PluginInstanceId::new(),
                    parameters: BTreeMap::new(),
                    bypass: false,
                    failure_policy: Default::default(),
                    switch_policy: Default::default(),
                    gain_db: 0.0,
                    mute: false,
                }],
                gain_db: 0.0,
                mute: false,
            },
        );
        assert_eq!(
            registered.instance_policy,
            InstancePolicy::UnlimitedInstances
        );
        let first = registered.instantiate();
        let second = registered.instantiate();
        let (
            NodeProperties::AudioProcessingGroup {
                flow: first_flow, ..
            },
            NodeProperties::AudioProcessingGroup {
                flow: second_flow, ..
            },
        ) = (&first.properties, &second.properties)
        else {
            panic!("expected group")
        };
        assert_ne!(first_flow[0].id, second_flow[0].id);
        assert_ne!(first_flow[0].instance_id, second_flow[0].instance_id);
    }
}

fn fresh_ports(ports: &[PortSpec]) -> Vec<PortSpec> {
    ports
        .iter()
        .map(|port| PortSpec {
            id: PortId::new(),
            name: port.name.clone(),
            direction: port.direction,
            signal_kind: port.signal_kind,
            event_types: port.event_types.clone(),
            plugin_port_key: port.plugin_port_key.clone(),
            max_connections: port.max_connections,
            required: port.required,
        })
        .collect()
}

fn resource_key(properties: &NodeProperties) -> Option<String> {
    match properties {
        NodeProperties::Microphone { endpoint_id, .. }
        | NodeProperties::EndpointLoopback { endpoint_id, .. }
        | NodeProperties::Monitor { endpoint_id, .. }
        | NodeProperties::VirtualDevice { endpoint_id, .. } => Some(endpoint_id.clone()),
        NodeProperties::ProcessPid { pid, .. } => Some(format!("pid:{pid}")),
        NodeProperties::ProcessExe { exe_path, .. } => Some(format!("exe:{}", exe_path.display())),
        NodeProperties::FileInput { path, .. } => Some(format!("file:{}", path.display())),
        NodeProperties::FolderReplay { path, .. } => Some(format!("folder:{}", path.display())),
        NodeProperties::MemoryReplay { cache_id, .. }
        | NodeProperties::ReplayCache { cache_id, .. } => Some(format!("cache:{cache_id}")),
        _ => None,
    }
}

#[derive(Default)]
pub struct NodeRegistry {
    nodes: RwLock<HashMap<RegisteredNodeId, RegisteredNode>>,
}

impl NodeRegistry {
    pub fn from_nodes(nodes: HashMap<RegisteredNodeId, RegisteredNode>) -> Self {
        Self {
            nodes: RwLock::new(nodes),
        }
    }

    pub fn snapshot(&self) -> HashMap<RegisteredNodeId, RegisteredNode> {
        self.nodes.read().clone()
    }

    pub fn replace_all(&self, nodes: HashMap<RegisteredNodeId, RegisteredNode>) {
        *self.nodes.write() = nodes;
    }

    pub fn list(&self) -> Vec<RegisteredNode> {
        let mut nodes: Vec<_> = self.nodes.read().values().cloned().collect();
        nodes.sort_by(|a, b| {
            a.playback_order
                .cmp(&b.playback_order)
                .then_with(|| a.display_name.cmp(&b.display_name))
        });
        nodes
    }

    /// Resolves the registration identity of a node imported from clipboard.
    ///
    /// The persisted registration id is authoritative only after its type,
    /// resource identity, and (for external transforms) plugin id have been
    /// checked. Legacy payloads without that id may use the original graph
    /// node id, then a unique resource/type match. Parameter values are
    /// deliberately excluded so editing gain or mute cannot evade limits.
    pub fn registered_identity_for_import(
        &self,
        node: &NodeSpec,
        graph: &GraphSnapshot,
    ) -> Option<RegisteredNodeId> {
        let nodes = self.nodes.read();
        if let Some(id) = node.registered_node_id {
            return nodes
                .get(&id)
                .filter(|registered| registration_matches(node, registered))
                .map(|_| id);
        }
        if let Some(id) = graph
            .nodes
            .get(&node.id)
            .and_then(|original| original.registered_node_id)
        {
            if nodes
                .get(&id)
                .is_some_and(|registered| registration_matches(node, registered))
            {
                return Some(id);
            }
        }
        let key = resource_key(&node.properties)?;
        let mut matches = nodes.values().filter(|registered| {
            registered.node_type == node.node_type
                && registered.resource_key.as_deref() == Some(key.as_str())
                && plugin_id_matches(node, registered)
        });
        let found = matches.next()?.id;
        matches.next().is_none().then_some(found)
    }

    /// Returns whether an unregistered clipboard node is intrinsically
    /// repeatable and therefore needs no registration identity.
    pub fn allows_unregistered_import(node: &NodeSpec) -> bool {
        match &node.properties {
            NodeProperties::AudioProcessingGroup { .. } | NodeProperties::MultiInput { .. } => true,
            NodeProperties::TransformPlugin { plugin_id, .. } => {
                plugin_id.starts_with("audio-router.builtin.")
            }
            NodeProperties::MemoryReplay {
                mode: MemoryReplayMode::SelectFromCache,
                ..
            } => true,
            _ => false,
        }
    }

    pub fn get(&self, id: RegisteredNodeId) -> Result<RegisteredNode> {
        self.nodes
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(id.to_string()))
    }

    pub fn register(&self, node: RegisteredNode) -> Result<RegisteredNode> {
        let mut nodes = self.nodes.write();
        if nodes.contains_key(&node.id) {
            return Err(BackendError::InvalidOperation(format!(
                "registered node {} already exists",
                node.id
            )));
        }
        nodes.insert(node.id, node.clone());
        Ok(node)
    }

    pub fn update(&self, node: RegisteredNode) -> Result<RegisteredNode> {
        let mut nodes = self.nodes.write();
        if !nodes.contains_key(&node.id) {
            return Err(BackendError::NodeNotFound(node.id.to_string()));
        }
        nodes.insert(node.id, node.clone());
        Ok(node)
    }

    pub fn delete(&self, id: RegisteredNodeId) -> Result<RegisteredNode> {
        self.nodes
            .write()
            .remove(&id)
            .ok_or_else(|| BackendError::NodeNotFound(id.to_string()))
    }

    pub fn duplicate(&self, id: RegisteredNodeId) -> Result<RegisteredNode> {
        let mut copy = self.get(id)?;
        copy.id = RegisteredNodeId::new();
        copy.display_name = format!("{} Copy", copy.display_name);
        copy.origin = RegistrationOrigin::User;
        self.register(copy)
    }

    pub fn instantiate(&self, id: RegisteredNodeId, graph: &GraphSnapshot) -> Result<NodeSpec> {
        let registered = self.get(id)?;
        if !registered.enabled {
            return Err(BackendError::InvalidOperation(
                "registered node is disabled".into(),
            ));
        }
        let memory_list_is_unlimited = matches!(
            &registered.properties,
            NodeProperties::MemoryReplay {
                mode: MemoryReplayMode::SelectFromCache,
                ..
            }
        );
        if registered.instance_policy == InstancePolicy::RegisteredInstanceOnly
            && !memory_list_is_unlimited
            && graph
                .nodes
                .values()
                .any(|node| node.registered_node_id == Some(id))
        {
            return Err(BackendError::InvalidOperation(
                "registered node is already placed in the graph".into(),
            ));
        }
        Ok(registered.instantiate())
    }
}

fn registration_matches(node: &NodeSpec, registered: &RegisteredNode) -> bool {
    node.node_type == registered.node_type
        && resource_key(&node.properties) == registered.resource_key
        && plugin_id_matches(node, registered)
}

fn plugin_id_matches(node: &NodeSpec, registered: &RegisteredNode) -> bool {
    match (&node.properties, &registered.properties) {
        (
            NodeProperties::TransformPlugin {
                plugin_id: actual, ..
            },
            NodeProperties::TransformPlugin {
                plugin_id: expected,
                ..
            },
        ) => actual == expected,
        _ => true,
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;

    fn microphone(endpoint_id: &str) -> NodeSpec {
        NodeSpec::new(
            NodeType::Microphone,
            "mic",
            NodeProperties::Microphone {
                endpoint_id: endpoint_id.into(),
                capture_method: Default::default(),
                gain_db: 0.0,
                mute: false,
            },
        )
    }

    #[test]
    fn import_identity_ignores_editable_gain_but_checks_registration_identity() {
        let mut source = microphone("capture://one");
        let mut registered = RegisteredNode::from_graph_node(&source);
        registered.id = RegisteredNodeId::new();
        source.registered_node_id = Some(registered.id);
        let registry = NodeRegistry::from_nodes(HashMap::from([(registered.id, registered)]));
        let mut imported = source.clone();
        if let NodeProperties::Microphone { gain_db, .. } = &mut imported.properties {
            *gain_db = -18.0;
        }
        assert_eq!(
            registry.registered_identity_for_import(&imported, &GraphSnapshot::default()),
            Some(source.registered_node_id.unwrap())
        );
        imported.registered_node_id = None;
        let mut graph = GraphSnapshot::default();
        graph.nodes.insert(imported.id, source);
        assert_eq!(
            registry.registered_identity_for_import(&imported, &graph),
            Some(graph.nodes[&imported.id].registered_node_id.unwrap())
        );
    }

    #[test]
    fn import_identity_rejects_external_plugin_definition_swap() {
        let source = RegisteredNode::user(
            "plugin",
            NodeProperties::TransformPlugin {
                plugin_id: "vendor.original".into(),
                instance_id: PluginInstanceId::new(),
                parameters: Default::default(),
                bypass: false,
                failure_policy: Default::default(),
                switch_policy: Default::default(),
                gain_db: 0.0,
                mute: false,
            },
        );
        let mut imported = source.instantiate();
        imported.registered_node_id = Some(source.id);
        if let NodeProperties::TransformPlugin { plugin_id, .. } = &mut imported.properties {
            *plugin_id = "vendor.replaced".into();
        }
        let registry = NodeRegistry::from_nodes(HashMap::from([(source.id, source)]));
        assert_eq!(
            registry.registered_identity_for_import(&imported, &GraphSnapshot::default()),
            None
        );
    }

    #[test]
    fn unregistered_repeatable_nodes_are_explicitly_allowed() {
        let group = NodeSpec::new(
            NodeType::AudioProcessingGroup,
            "group",
            NodeProperties::AudioProcessingGroup {
                flow: Vec::new(),
                gain_db: 0.0,
                mute: false,
            },
        );
        assert!(NodeRegistry::allows_unregistered_import(&group));
        let plugin = NodeSpec::new(
            NodeType::TransformPlugin,
            "builtin",
            NodeProperties::TransformPlugin {
                plugin_id: "audio-router.builtin.compressor".into(),
                instance_id: PluginInstanceId::new(),
                parameters: Default::default(),
                bypass: false,
                failure_policy: Default::default(),
                switch_policy: Default::default(),
                gain_db: 0.0,
                mute: false,
            },
        );
        assert!(NodeRegistry::allows_unregistered_import(&plugin));
    }
}
