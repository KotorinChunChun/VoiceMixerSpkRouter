use std::sync::Arc;

use arc_swap::ArcSwap;
use parking_lot::RwLock;

use crate::audio::graph::{
    ConnectionEndpoint, Edge, EdgeId, EventEdge, GraphSnapshot, NodeId, NodeLayout, NodeProperties,
    NodeSpec, NodeType, PortDirection, PortId, RegisteredNodeId, SignalKind,
};
use crate::error::{BackendError, Result};
use crate::registry::node_registry::RegisteredNode;
use crate::routing::validator::{
    validate_connection_candidate, validate_graph, ConnectionCandidateResult, CyclePolicy,
    ValidationReport,
};

pub struct GraphController {
    graph: RwLock<GraphSnapshot>,
    runtime_snapshot: Arc<ArcSwap<GraphSnapshot>>,
}

impl Default for GraphController {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphController {
    pub fn new() -> Self {
        let graph = GraphSnapshot::default();
        Self {
            graph: RwLock::new(graph.clone()),
            runtime_snapshot: Arc::new(ArcSwap::from_pointee(graph)),
        }
    }

    pub fn runtime_snapshot(&self) -> Arc<ArcSwap<GraphSnapshot>> {
        self.runtime_snapshot.clone()
    }

    pub fn snapshot(&self) -> GraphSnapshot {
        self.graph.read().clone()
    }

    pub fn list_nodes(&self) -> Vec<NodeSpec> {
        self.graph.read().nodes.values().cloned().collect()
    }

    pub fn list_edges(&self) -> Vec<Edge> {
        self.graph.read().edges.clone()
    }

    pub fn list_event_edges(&self) -> Vec<EventEdge> {
        self.graph.read().event_edges.clone()
    }

    pub fn create_node(&self, node: NodeSpec) -> Result<NodeSpec> {
        self.mutate(|g| {
            if g.nodes.contains_key(&node.id) {
                return Err(BackendError::InvalidOperation(format!(
                    "node {} already exists",
                    node.id
                )));
            }
            g.nodes.insert(node.id, node.clone());
            Ok(node)
        })
    }

    pub fn update_node(&self, node: NodeSpec) -> Result<NodeSpec> {
        self.mutate(|g| {
            if !g.nodes.contains_key(&node.id) {
                return Err(BackendError::NodeNotFound(node.id.to_string()));
            }
            g.nodes.insert(node.id, node.clone());
            Ok(node)
        })
    }

    /// Applies the registered definition's display default to every graph
    /// instance created from it in one graph revision.
    pub fn set_registered_playback_display(
        &self,
        registered_node_id: RegisteredNodeId,
        show_in_playback: bool,
    ) -> Result<Vec<NodeSpec>> {
        self.mutate(|graph| {
            let mut updated = Vec::new();
            for node in graph.nodes.values_mut() {
                if node.registered_node_id == Some(registered_node_id) {
                    node.show_in_playback = show_in_playback;
                    updated.push(node.clone());
                }
            }
            Ok(updated)
        })
    }

    pub fn delete_node(&self, node_id: NodeId) -> Result<()> {
        self.mutate(|g| {
            if g.nodes.remove(&node_id).is_none() {
                return Err(BackendError::NodeNotFound(node_id.to_string()));
            }
            let removed_edges: Vec<_> = g
                .edges
                .iter()
                .filter(|edge| edge.from_node == node_id || edge.to_node == node_id)
                .cloned()
                .collect();
            g.edges
                .retain(|e| e.from_node != node_id && e.to_node != node_id);
            g.event_edges
                .retain(|e| e.from_node != node_id && e.to_node != node_id);
            for edge in removed_edges {
                if edge.to_node != node_id
                    && !g
                        .edges
                        .iter()
                        .any(|remaining| remaining.to_port == edge.to_port)
                {
                    remove_dynamic_input_port(g, edge.to_node, edge.to_port);
                }
            }
            g.layouts.remove(&node_id);
            Ok(())
        })
    }

    pub fn update_node_layout(&self, layout: NodeLayout) -> Result<NodeLayout> {
        layout.validate().map_err(BackendError::InvalidOperation)?;
        // 配置はUI専用のメタデータで、DSPの実行計画には含まれない。
        // `mutate` を使うとGraph revisionが進み、実行中のTransform plugin
        // に接続Portを再通知してしまう。RVCのようなprocess pluginでは、
        // ノード移動だけでその制御通信が失敗することがあるため、ここでは
        // runtime snapshotを更新しない。
        let mut graph = self.graph.write();
        if !graph.nodes.contains_key(&layout.node_id) {
            return Err(BackendError::NodeNotFound(layout.node_id.to_string()));
        }
        graph.layouts.insert(layout.node_id, layout);
        Ok(layout)
    }

    pub fn connect_nodes(
        &self,
        from_node: NodeId,
        from_port: PortId,
        to_node: NodeId,
        to_port: PortId,
    ) -> Result<Edge> {
        self.mutate(|g| connect_in_graph(g, from_node, Some(from_port), to_node, Some(to_port)))
    }

    pub fn connect_nodes_auto(&self, from_node: NodeId, to_node: NodeId) -> Result<Edge> {
        self.mutate(|g| connect_in_graph(g, from_node, None, to_node, None))
    }

    pub fn connect_endpoints(
        &self,
        first: ConnectionEndpoint,
        second: ConnectionEndpoint,
    ) -> Result<Edge> {
        let (source, destination) = match (first.direction, second.direction) {
            (PortDirection::Output, PortDirection::Input) => (first, second),
            (PortDirection::Input, PortDirection::Output) => (second, first),
            _ => {
                return Err(BackendError::InvalidOperation(
                    "connection endpoints must contain one output and one input".into(),
                ))
            }
        };
        self.mutate(|g| {
            connect_in_graph(
                g,
                source.node_id,
                source.port_id,
                destination.node_id,
                destination.port_id,
            )
        })
    }

    pub fn connect_event_nodes(
        &self,
        from_node: NodeId,
        from_port: PortId,
        to_node: NodeId,
        to_port: PortId,
    ) -> Result<EventEdge> {
        self.mutate(|graph| connect_event_in_graph(graph, from_node, from_port, to_node, to_port))
    }

    pub fn connect_event_endpoints(
        &self,
        first: ConnectionEndpoint,
        second: ConnectionEndpoint,
    ) -> Result<EventEdge> {
        let (source, destination) = match (first.direction, second.direction) {
            (PortDirection::Output, PortDirection::Input) => (first, second),
            (PortDirection::Input, PortDirection::Output) => (second, first),
            _ => {
                return Err(BackendError::InvalidOperation(
                    "event connection requires one output and one input".into(),
                ))
            }
        };
        let source_port = source
            .port_id
            .ok_or_else(|| BackendError::PortNotFound("event source port".into()))?;
        let destination_port = destination
            .port_id
            .ok_or_else(|| BackendError::PortNotFound("event destination port".into()))?;
        self.connect_event_nodes(
            source.node_id,
            source_port,
            destination.node_id,
            destination_port,
        )
    }

    pub fn validate_connection_candidate(
        &self,
        source: NodeId,
        destination: NodeId,
        cycle_policy: CyclePolicy,
    ) -> ConnectionCandidateResult {
        validate_connection_candidate(&self.graph.read(), source, destination, cycle_policy)
    }

    pub fn disconnect_nodes(&self, edge_id: EdgeId) -> Result<()> {
        self.disconnect_edge(edge_id)
    }

    pub fn disconnect_edge(&self, edge_id: EdgeId) -> Result<()> {
        self.mutate(|g| {
            if let Some(index) = g.edges.iter().position(|edge| edge.id == edge_id) {
                let edge = g.edges.remove(index);
                if !g
                    .edges
                    .iter()
                    .any(|remaining| remaining.to_port == edge.to_port)
                {
                    remove_dynamic_input_port(g, edge.to_node, edge.to_port);
                }
                return Ok(());
            }
            let index = g
                .event_edges
                .iter()
                .position(|edge| edge.id == edge_id)
                .ok_or_else(|| BackendError::EdgeNotFound(edge_id.to_string()))?;
            g.event_edges.remove(index);
            Ok(())
        })
    }

    pub fn set_edge_controls(&self, edge_id: EdgeId, gain_db: f32, mute: bool) -> Result<Edge> {
        if !gain_db.is_finite() {
            return Err(BackendError::InvalidOperation(
                "edge gain must be finite".into(),
            ));
        }
        self.mutate(|g| {
            let edge = g
                .edges
                .iter_mut()
                .find(|edge| edge.id == edge_id)
                .ok_or_else(|| BackendError::EdgeNotFound(edge_id.to_string()))?;
            edge.gain_db = gain_db;
            edge.mute = mute;
            let updated = edge.clone();
            if let Some(node) = g.nodes.get_mut(&updated.to_node) {
                if let NodeProperties::MultiInput { slots, .. } = &mut node.properties {
                    if let Some(slot) = slots
                        .iter_mut()
                        .find(|slot| slot.port_id == updated.to_port)
                    {
                        slot.edge_id = Some(updated.id);
                        slot.gain_db = gain_db;
                        slot.mute = mute;
                    }
                }
            }
            Ok(updated)
        })
    }

    pub fn set_edge_color(&self, edge_id: EdgeId, color: Option<String>) -> Result<Edge> {
        if color.as_ref().is_some_and(|value| {
            value.len() != 7
                || !value.starts_with('#')
                || !value[1..].bytes().all(|c| c.is_ascii_hexdigit())
        }) {
            return Err(BackendError::InvalidOperation(
                "線の色は#RRGGBB形式で指定してください".into(),
            ));
        }
        self.mutate(|g| {
            let edge = g
                .edges
                .iter_mut()
                .find(|edge| edge.id == edge_id)
                .ok_or_else(|| BackendError::EdgeNotFound(edge_id.to_string()))?;
            edge.color = color;
            Ok(edge.clone())
        })
    }

    pub fn set_runtime_enabled(&self, node_id: NodeId, enabled: bool) -> Result<NodeSpec> {
        self.mutate(|g| {
            let node = g
                .nodes
                .get_mut(&node_id)
                .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
            node.runtime_enabled = enabled;
            Ok(node.clone())
        })
    }

    pub fn duplicate_node_detached(&self, node_id: NodeId) -> Result<NodeSpec> {
        let source = self
            .graph
            .read()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        let mut definition = RegisteredNode::from_graph_node(&source);
        definition.id = RegisteredNodeId::new();
        definition.display_name = format!("{} Copy", source.display_name);
        let mut copy = definition.instantiate();
        copy.registered_node_id = None;
        self.create_node(copy)
    }

    pub fn remove_port(&self, node_id: NodeId, port_id: PortId) -> Result<()> {
        self.mutate(|g| {
            let node = g
                .nodes
                .get_mut(&node_id)
                .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
            node.input_ports.retain(|p| p.id != port_id);
            node.output_ports.retain(|p| p.id != port_id);
            if let crate::audio::graph::NodeProperties::MultiInput { slots, .. } =
                &mut node.properties
            {
                slots.retain(|s| s.port_id != port_id);
            }
            if let NodeProperties::InputSelector { config, .. } = &mut node.properties {
                if config.active_port_id == Some(port_id) {
                    config.active_port_id = None;
                }
            }
            g.edges
                .retain(|e| e.from_port != port_id && e.to_port != port_id);
            g.event_edges
                .retain(|e| e.from_port != port_id && e.to_port != port_id);
            if node.input_ports.is_empty() {
                if node.node_type == NodeType::MultiInput {
                    node.add_multi_input_port("input 1");
                } else if node.node_type == NodeType::InputSelector {
                    node.add_input_selector_port("input 1");
                }
            }
            Ok(())
        })
    }

    pub fn validate_graph(&self) -> ValidationReport {
        validate_graph(&self.graph.read())
    }

    pub fn replace_graph(&self, mut next: GraphSnapshot) -> Result<()> {
        let report = validate_graph(&next);
        if !report.is_valid() {
            return Err(BackendError::GraphValidation(
                report
                    .issues
                    .iter()
                    .filter(|i| i.severity == crate::routing::validator::ValidationSeverity::Error)
                    .map(|i| i.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let mut guard = self.graph.write();
        next.revision = guard.revision.saturating_add(1);
        *guard = next.clone();
        self.runtime_snapshot.store(Arc::new(next));
        Ok(())
    }

    /// Graphを公開する前に永続化する。失敗時は旧Graphを保ち、他threadの更新を巻き戻さない。
    pub(crate) fn commit_graph_candidate(
        &self,
        expected_revision: u64,
        mut candidate: GraphSnapshot,
        persist: impl FnOnce(&GraphSnapshot) -> Result<()>,
    ) -> Result<()> {
        let mut guard = self.graph.write();
        if guard.revision != expected_revision {
            return Err(BackendError::InvalidOperation(
                "Graphが更新されました。再同期して追加してください".into(),
            ));
        }
        let report = validate_graph(&candidate);
        let errors: Vec<_> = report
            .issues
            .iter()
            .filter(|issue| {
                issue.severity == crate::routing::validator::ValidationSeverity::Error
                    && issue.code != "required_plugin_input_unconnected"
            })
            .map(|issue| issue.message.clone())
            .collect();
        if !errors.is_empty() {
            return Err(BackendError::GraphValidation(errors.join("; ")));
        }
        candidate.revision = guard.revision.saturating_add(1);
        persist(&candidate)?;
        *guard = candidate.clone();
        self.runtime_snapshot.store(Arc::new(candidate));
        Ok(())
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut GraphSnapshot) -> Result<T>) -> Result<T> {
        let mut guard = self.graph.write();
        let mut candidate = guard.clone();
        let out = f(&mut candidate)?;
        let report = validate_graph(&candidate);
        let has_blocking_error = report.issues.iter().any(|issue| {
            issue.severity == crate::routing::validator::ValidationSeverity::Error
                && issue.code != "required_plugin_input_unconnected"
        });
        if has_blocking_error {
            return Err(BackendError::GraphValidation(
                report
                    .issues
                    .iter()
                    .filter(|i| {
                        i.severity == crate::routing::validator::ValidationSeverity::Error
                            && i.code != "required_plugin_input_unconnected"
                    })
                    .map(|i| i.message.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        candidate.revision = guard.revision.saturating_add(1);
        *guard = candidate.clone();
        self.runtime_snapshot.store(Arc::new(candidate));
        Ok(out)
    }
}

fn connect_in_graph(
    graph: &mut GraphSnapshot,
    from_node: NodeId,
    requested_from_port: Option<PortId>,
    to_node: NodeId,
    requested_to_port: Option<PortId>,
) -> Result<Edge> {
    let assessment =
        validate_connection_candidate(graph, from_node, to_node, CyclePolicy::WarningOnly);
    if !assessment.allowed {
        return Err(BackendError::InvalidOperation(format!(
            "connection rejected: {:?}",
            assessment.reason
        )));
    }

    let from = graph
        .nodes
        .get(&from_node)
        .ok_or_else(|| BackendError::NodeNotFound(from_node.to_string()))?;
    let from_port = requested_from_port
        .or_else(|| from.output_ports.first().map(|port| port.id))
        .ok_or_else(|| BackendError::PortNotFound("source output port".into()))?;
    if !from.has_port(from_port, PortDirection::Output) {
        return Err(BackendError::PortNotFound(from_port.to_string()));
    }
    let from_signal_kind = from
        .output_ports
        .iter()
        .find(|port| port.id == from_port)
        .map(|port| port.signal_kind)
        .ok_or_else(|| BackendError::PortNotFound(from_port.to_string()))?;
    if from_signal_kind != SignalKind::Audio {
        return Err(BackendError::InvalidOperation(
            "event ports must use connect_event_nodes".into(),
        ));
    }

    let used: std::collections::HashMap<PortId, usize> =
        graph
            .edges
            .iter()
            .fold(std::collections::HashMap::new(), |mut counts, edge| {
                *counts.entry(edge.to_port).or_default() += 1;
                counts
            });
    let to = graph
        .nodes
        .get(&to_node)
        .ok_or_else(|| BackendError::NodeNotFound(to_node.to_string()))?;
    let to_port = if let Some(port_id) = requested_to_port {
        let port = to
            .input_ports
            .iter()
            .find(|port| port.id == port_id)
            .ok_or_else(|| BackendError::PortNotFound(port_id.to_string()))?;
        if port.signal_kind != from_signal_kind {
            return Err(BackendError::InvalidOperation(
                "source and destination signal kinds differ".into(),
            ));
        }
        if used.get(&port_id).copied().unwrap_or(0) >= port.max_connections.unwrap_or(usize::MAX) {
            return Err(BackendError::InvalidOperation(
                "destination input port is full".into(),
            ));
        }
        port_id
    } else if let Some(port) = to.input_ports.iter().find(|port| {
        port.signal_kind == from_signal_kind
            && used.get(&port.id).copied().unwrap_or(0) < port.max_connections.unwrap_or(usize::MAX)
    }) {
        port.id
    } else if matches!(to.node_type, NodeType::MultiInput | NodeType::InputSelector) {
        let name = format!("input {}", to.input_ports.len() + 1);
        let node = graph
            .nodes
            .get_mut(&to_node)
            .ok_or_else(|| BackendError::NodeNotFound(to_node.to_string()))?;
        (if node.node_type == NodeType::MultiInput {
            node.add_multi_input_port(name)
        } else {
            node.add_input_selector_port(name)
        })
        .ok_or_else(|| BackendError::InvalidOperation("failed to add dynamic input port".into()))?
    } else {
        return Err(BackendError::InvalidOperation(
            "destination input is full".into(),
        ));
    };

    let edge = Edge::new(from_node, from_port, to_node, to_port);
    if let Some(node) = graph.nodes.get_mut(&to_node) {
        if let NodeProperties::MultiInput { slots, .. } = &mut node.properties {
            if let Some(slot) = slots.iter_mut().find(|slot| slot.port_id == to_port) {
                slot.edge_id = Some(edge.id);
                slot.gain_db = edge.gain_db;
                slot.mute = edge.mute;
            }
        }
    }
    graph.edges.push(edge.clone());
    Ok(edge)
}

fn connect_event_in_graph(
    graph: &mut GraphSnapshot,
    from_node: NodeId,
    from_port: PortId,
    to_node: NodeId,
    to_port: PortId,
) -> Result<EventEdge> {
    let source = graph
        .nodes
        .get(&from_node)
        .ok_or_else(|| BackendError::NodeNotFound(from_node.to_string()))?;
    let source_port = source
        .output_ports
        .iter()
        .find(|port| port.id == from_port)
        .ok_or_else(|| BackendError::PortNotFound(from_port.to_string()))?;
    let destination = graph
        .nodes
        .get(&to_node)
        .ok_or_else(|| BackendError::NodeNotFound(to_node.to_string()))?;
    let destination_port = destination
        .input_ports
        .iter()
        .find(|port| port.id == to_port)
        .ok_or_else(|| BackendError::PortNotFound(to_port.to_string()))?;
    if source_port.signal_kind != SignalKind::Event
        || destination_port.signal_kind != SignalKind::Event
    {
        return Err(BackendError::InvalidOperation(
            "source and destination must both be Event ports".into(),
        ));
    }
    if source_port.event_types.is_empty()
        || (!destination_port.event_types.is_empty()
            && !source_port
                .event_types
                .iter()
                .any(|kind| destination_port.event_types.contains(kind)))
    {
        return Err(BackendError::InvalidOperation(
            "event type is not supported by destination".into(),
        ));
    }
    if graph.event_edges.iter().any(|edge| {
        edge.from_node == from_node
            && edge.from_port == from_port
            && edge.to_node == to_node
            && edge.to_port == to_port
    }) {
        return Err(BackendError::InvalidOperation(
            "event edge already exists".into(),
        ));
    }
    let used = graph
        .event_edges
        .iter()
        .filter(|edge| edge.to_port == to_port)
        .count();
    if used >= destination_port.max_connections.unwrap_or(usize::MAX) {
        return Err(BackendError::InvalidOperation(
            "destination event port is full".into(),
        ));
    }
    let edge = EventEdge::new(from_node, from_port, to_node, to_port);
    graph.event_edges.push(edge.clone());
    Ok(edge)
}

fn remove_dynamic_input_port(graph: &mut GraphSnapshot, node_id: NodeId, port_id: PortId) {
    let Some(node) = graph.nodes.get_mut(&node_id) else {
        return;
    };
    if !matches!(
        node.node_type,
        NodeType::MultiInput | NodeType::InputSelector
    ) {
        return;
    }
    node.input_ports.retain(|port| port.id != port_id);
    if let NodeProperties::MultiInput { slots, .. } = &mut node.properties {
        slots.retain(|slot| slot.port_id != port_id);
    }
    if let NodeProperties::InputSelector { config, .. } = &mut node.properties {
        if config.active_port_id == Some(port_id) {
            config.active_port_id = None;
        }
    }
    if node.input_ports.is_empty() {
        if node.node_type == NodeType::MultiInput {
            node.add_multi_input_port("input 1");
        } else {
            node.add_input_selector_port("input 1");
        }
    }
}
