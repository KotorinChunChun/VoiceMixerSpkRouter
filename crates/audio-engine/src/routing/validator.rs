use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::audio::graph::{GraphSnapshot, NodeClass, NodeId, NodeType, SignalKind};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionValidation {
    Valid,
    Duplicate,
    SourceCannotOutput,
    DestinationCannotInput,
    DestinationInputFull,
    PortNotFound,
    NodeDisabled,
    WouldCreateCycle,
    InvalidNodeType,
    SignalKindMismatch,
    EventTypeUnsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ConnectionCandidateResult {
    pub allowed: bool,
    pub reason: ConnectionValidation,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CyclePolicy {
    WarningOnly,
    Reject,
}

impl Default for CyclePolicy {
    fn default() -> Self {
        Self::WarningOnly
    }
}

pub fn validate_connection_candidate(
    graph: &GraphSnapshot,
    source: NodeId,
    destination: NodeId,
    cycle_policy: CyclePolicy,
) -> ConnectionCandidateResult {
    let Some(from) = graph.nodes.get(&source) else {
        return candidate(
            false,
            ConnectionValidation::InvalidNodeType,
            Some("source node not found"),
        );
    };
    let Some(to) = graph.nodes.get(&destination) else {
        return candidate(
            false,
            ConnectionValidation::InvalidNodeType,
            Some("destination node not found"),
        );
    };
    if !from.enabled || !to.enabled {
        return candidate(false, ConnectionValidation::NodeDisabled, None);
    }
    if from.node_type.class() == NodeClass::Output || from.output_ports.is_empty() {
        return candidate(false, ConnectionValidation::SourceCannotOutput, None);
    }
    if to.node_type.class() == NodeClass::Input
        || (to.input_ports.is_empty() && to.node_type != NodeType::MultiInput)
    {
        return candidate(false, ConnectionValidation::DestinationCannotInput, None);
    }
    let compatible_signal = from.output_ports.iter().any(|output| {
        to.input_ports
            .iter()
            .any(|input| input.signal_kind == output.signal_kind)
    });
    if !compatible_signal {
        return candidate(false, ConnectionValidation::SignalKindMismatch, None);
    }
    let event_pairs: Vec<_> = from
        .output_ports
        .iter()
        .filter(|port| port.signal_kind == SignalKind::Event)
        .flat_map(|output| {
            to.input_ports
                .iter()
                .filter(|input| input.signal_kind == SignalKind::Event)
                .map(move |input| (output, input))
        })
        .collect();
    let has_audio_pair = from.output_ports.iter().any(|output| {
        output.signal_kind == SignalKind::Audio
            && to
                .input_ports
                .iter()
                .any(|input| input.signal_kind == SignalKind::Audio)
    });
    if !has_audio_pair
        && !event_pairs.iter().any(|(output, input)| {
            !output.event_types.is_empty()
                && (input.event_types.is_empty()
                    || output
                        .event_types
                        .iter()
                        .any(|kind| input.event_types.contains(kind)))
        })
    {
        return candidate(false, ConnectionValidation::EventTypeUnsupported, None);
    }
    if !has_audio_pair {
        let compatible_inputs: Vec<_> = event_pairs
            .iter()
            .filter(|(output, input)| {
                !output.event_types.is_empty()
                    && (input.event_types.is_empty()
                        || output
                            .event_types
                            .iter()
                            .any(|kind| input.event_types.contains(kind)))
            })
            .map(|(_, input)| *input)
            .collect();
        if compatible_inputs.iter().all(|input| {
            graph
                .event_edges
                .iter()
                .filter(|edge| edge.to_port == input.id)
                .count()
                >= input.max_connections.unwrap_or(usize::MAX)
        }) {
            return candidate(false, ConnectionValidation::DestinationInputFull, None);
        }
    }
    if graph
        .edges
        .iter()
        .any(|edge| edge.from_node == source && edge.to_node == destination)
    {
        return candidate(false, ConnectionValidation::Duplicate, None);
    }
    if graph
        .event_edges
        .iter()
        .any(|edge| edge.from_node == source && edge.to_node == destination)
    {
        return candidate(false, ConnectionValidation::Duplicate, None);
    }
    if !matches!(to.node_type, NodeType::MultiInput | NodeType::InputSelector) {
        let total_max = to
            .input_ports
            .iter()
            .map(|port| port.max_connections.unwrap_or(usize::MAX))
            .fold(0usize, usize::saturating_add);
        if graph.incoming(destination).count() >= total_max {
            return candidate(false, ConnectionValidation::DestinationInputFull, None);
        }
    }
    if crate::routing::traversal::path_exists(graph, destination, source) {
        return candidate(
            cycle_policy == CyclePolicy::WarningOnly,
            ConnectionValidation::WouldCreateCycle,
            Some("connection would create a cycle"),
        );
    }
    candidate(true, ConnectionValidation::Valid, None)
}

fn candidate(
    allowed: bool,
    reason: ConnectionValidation,
    warning: Option<&str>,
) -> ConnectionCandidateResult {
    ConnectionCandidateResult {
        allowed,
        reason,
        warning: warning.map(str::to_string),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ValidationIssue {
    pub code: String,
    pub message: String,
    pub severity: ValidationSeverity,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValidationSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|x| x.severity == ValidationSeverity::Error)
    }

    pub fn error(&mut self, code: &str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            code: code.into(),
            message: message.into(),
            severity: ValidationSeverity::Error,
        });
    }

    pub fn warning(&mut self, code: &str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            code: code.into(),
            message: message.into(),
            severity: ValidationSeverity::Warning,
        });
    }
}

pub fn validate_graph(graph: &GraphSnapshot) -> ValidationReport {
    let mut report = ValidationReport::default();
    let mut edge_ids = HashSet::new();
    let mut endpoint_pairs = HashSet::new();
    let mut event_endpoint_pairs = HashSet::new();

    for edge in &graph.edges {
        if !edge_ids.insert(edge.id) {
            report.error(
                "duplicate_edge_id",
                format!("duplicate edge id {}", edge.id),
            );
        }
        if !endpoint_pairs.insert((edge.from_node, edge.to_node)) {
            report.error(
                "duplicate_connection",
                format!(
                    "duplicate connection {} -> {}",
                    edge.from_node, edge.to_node
                ),
            );
        }
        if !edge.gain_db.is_finite() {
            report.error(
                "invalid_edge_gain",
                format!("edge {} gain is not finite", edge.id),
            );
        }
        let Some(from) = graph.nodes.get(&edge.from_node) else {
            report.error(
                "missing_from_node",
                format!("edge {} source node missing", edge.id),
            );
            continue;
        };
        let Some(to) = graph.nodes.get(&edge.to_node) else {
            report.error(
                "missing_to_node",
                format!("edge {} destination node missing", edge.id),
            );
            continue;
        };
        let from_port = from
            .output_ports
            .iter()
            .find(|port| port.id == edge.from_port);
        if from_port.is_none() {
            report.error(
                "invalid_from_port",
                format!("edge {} source port is invalid", edge.id),
            );
        }
        let to_port = to.input_ports.iter().find(|port| port.id == edge.to_port);
        if to_port.is_none() {
            report.error(
                "invalid_to_port",
                format!("edge {} destination port is invalid", edge.id),
            );
        }
        if let (Some(from_port), Some(to_port)) = (from_port, to_port) {
            if from_port.signal_kind != to_port.signal_kind {
                report.error(
                    "signal_kind_mismatch",
                    format!("edge {} connects different signal kinds", edge.id),
                );
            }
            if from_port.signal_kind != SignalKind::Audio {
                report.error(
                    "event_edge_in_audio_graph",
                    format!("edge {} must connect Audio ports", edge.id),
                );
            }
        }
        if from.node_type.class() == NodeClass::Output {
            report.error(
                "output_has_outgoing",
                format!(
                    "output node {} cannot have outgoing edges",
                    from.display_name
                ),
            );
        }
        if to.node_type.class() == NodeClass::Input {
            report.error(
                "input_has_incoming",
                format!("input node {} cannot have incoming edges", to.display_name),
            );
        }
    }

    for edge in &graph.event_edges {
        if !edge_ids.insert(edge.id) {
            report.error(
                "duplicate_edge_id",
                format!("duplicate edge id {}", edge.id),
            );
        }
        if !event_endpoint_pairs.insert((
            edge.from_node,
            edge.from_port,
            edge.to_node,
            edge.to_port,
        )) {
            report.error(
                "duplicate_event_connection",
                format!("duplicate event connection for edge {}", edge.id),
            );
        }
        let Some(from) = graph.nodes.get(&edge.from_node) else {
            report.error(
                "missing_event_from_node",
                format!("event edge {} source node missing", edge.id),
            );
            continue;
        };
        let Some(to) = graph.nodes.get(&edge.to_node) else {
            report.error(
                "missing_event_to_node",
                format!("event edge {} destination node missing", edge.id),
            );
            continue;
        };
        let from_port = from
            .output_ports
            .iter()
            .find(|port| port.id == edge.from_port);
        let to_port = to.input_ports.iter().find(|port| port.id == edge.to_port);
        match (from_port, to_port) {
            (Some(from_port), Some(to_port)) => {
                if from_port.signal_kind != SignalKind::Event
                    || to_port.signal_kind != SignalKind::Event
                {
                    report.error(
                        "signal_kind_mismatch",
                        format!("event edge {} must connect Event ports", edge.id),
                    );
                } else if from_port.event_types.is_empty()
                    || (!to_port.event_types.is_empty()
                        && !from_port
                            .event_types
                            .iter()
                            .any(|kind| to_port.event_types.contains(kind)))
                {
                    report.error(
                        "event_type_unsupported",
                        format!("event edge {} has no compatible event type", edge.id),
                    );
                }
            }
            (None, _) => report.error(
                "invalid_event_from_port",
                format!("event edge {} source port is invalid", edge.id),
            ),
            (_, None) => report.error(
                "invalid_event_to_port",
                format!("event edge {} destination port is invalid", edge.id),
            ),
        }
    }

    let mut event_input_counts: HashMap<_, usize> = HashMap::new();
    for edge in &graph.event_edges {
        *event_input_counts.entry(edge.to_port).or_default() += 1;
    }
    for node in graph.nodes.values() {
        for port in &node.input_ports {
            if port.signal_kind == SignalKind::Event {
                let count = event_input_counts.get(&port.id).copied().unwrap_or(0);
                if count > port.max_connections.unwrap_or(usize::MAX) {
                    report.error(
                        "event_port_connection_limit",
                        format!(
                            "Event input {} has {} connections but allows {:?}",
                            port.name, count, port.max_connections
                        ),
                    );
                }
            }
        }
    }

    for node in graph.nodes.values() {
        for port in node.input_ports.iter().chain(&node.output_ports) {
            if port.signal_kind == SignalKind::Audio && !port.event_types.is_empty() {
                report.error(
                    "audio_port_has_event_types",
                    format!("Audio port {} must not declare event types", port.name),
                );
            }
            if port.signal_kind == SignalKind::Event
                && port.direction == crate::audio::graph::PortDirection::Output
                && port.event_types.is_empty()
            {
                report.error(
                    "event_output_has_no_types",
                    format!("Event output {} must declare event types", port.name),
                );
            }
        }
        if node.properties.node_type() != node.node_type {
            report.error(
                "node_property_type_mismatch",
                format!(
                    "node {} is {:?} but carries {:?} properties",
                    node.display_name,
                    node.node_type,
                    node.properties.node_type()
                ),
            );
        }
        let incoming = graph.incoming(node.id).count();
        let outgoing = graph.outgoing(node.id).count();
        match node.node_type.class() {
            NodeClass::Input if incoming > 0 => report.error(
                "input_has_incoming",
                format!("{} has {} incoming edges", node.display_name, incoming),
            ),
            NodeClass::Output if outgoing > 0 => report.error(
                "output_has_outgoing",
                format!("{} has {} outgoing edges", node.display_name, outgoing),
            ),
            _ => {}
        }
        if node.node_type == NodeType::Rvc && incoming > 1 {
            report.error(
                "rvc_multiple_inputs",
                format!(
                    "legacy RVC node {} accepts only one input",
                    node.display_name
                ),
            );
        }
        if node.node_type == NodeType::TransformPlugin {
            let mut per_port: HashMap<_, usize> = HashMap::new();
            for edge in graph.incoming(node.id) {
                *per_port.entry(edge.to_port).or_default() += 1;
            }
            for port in &node.input_ports {
                let count = per_port.get(&port.id).copied().unwrap_or(0);
                if port.required && count == 0 {
                    report.error(
                        "required_plugin_input_unconnected",
                        format!(
                            "transform plugin {} required input port {} is unconnected",
                            node.display_name, port.name
                        ),
                    );
                }
                if let Some(max) = port.max_connections {
                    if count > max {
                        report.error(
                            "plugin_port_connection_limit",
                            format!(
                                "transform plugin {} input port {} has {} connections but allows {}",
                                node.display_name, port.name, count, max
                            ),
                        );
                    }
                }
            }
        }
        if node.node_type.class() == NodeClass::Output && incoming > 1 {
            report.error(
                "output_multiple_inputs",
                format!(
                    "output node {} accepts only one input; add a Multi Input node",
                    node.display_name
                ),
            );
        }
        if node.node_type == NodeType::MultiInput {
            let ids: HashSet<_> = node.input_ports.iter().map(|p| p.id).collect();
            if let crate::audio::graph::NodeProperties::MultiInput { slots, .. } = &node.properties
            {
                for slot in slots {
                    if !ids.contains(&slot.port_id) {
                        report.error(
                            "orphan_mix_slot",
                            format!(
                                "Multi Input {} contains a slot for a missing port",
                                node.display_name
                            ),
                        );
                    }
                    if let Some(edge_id) = slot.edge_id {
                        if !graph.edges.iter().any(|edge| {
                            edge.id == edge_id
                                && edge.to_node == node.id
                                && edge.to_port == slot.port_id
                        }) {
                            report.error(
                                "orphan_mix_edge",
                                format!(
                                    "Multi Input {} slot references missing edge {}",
                                    node.display_name, edge_id
                                ),
                            );
                        }
                    }
                }
                for port in &node.input_ports {
                    if !slots.iter().any(|slot| slot.port_id == port.id) {
                        report.error(
                            "missing_mix_slot",
                            format!(
                                "Multi Input {} port {} has no channel settings",
                                node.display_name, port.name
                            ),
                        );
                    }
                }
            }
        }
        match &node.properties {
            crate::audio::graph::NodeProperties::TestSignal { config, .. } => {
                if let Err(message) = config.validate() {
                    report.error("invalid_test_signal_config", message);
                }
            }
            crate::audio::graph::NodeProperties::Delay { config, .. } => {
                if let Err(message) = config.validate() {
                    report.error("invalid_delay_config", message);
                }
            }
            crate::audio::graph::NodeProperties::ChannelMapper { config, .. } => {
                if let Err(message) = config.validate() {
                    report.error("invalid_channel_mapper_config", message);
                }
            }
            crate::audio::graph::NodeProperties::InputSelector { config, .. } => {
                if config
                    .active_port_id
                    .is_some_and(|port_id| !node.input_ports.iter().any(|port| port.id == port_id))
                {
                    report.error(
                        "invalid_selector_input",
                        "input selector active port does not belong to the node",
                    );
                }
            }
            crate::audio::graph::NodeProperties::SilenceDetector { config } => {
                if let Err(message) = config.validate() {
                    report.error("invalid_silence_detector_config", message);
                }
                let audio_inputs = node
                    .input_ports
                    .iter()
                    .filter(|port| port.signal_kind == SignalKind::Audio)
                    .count();
                let event_outputs: Vec<_> = node
                    .output_ports
                    .iter()
                    .filter(|port| port.signal_kind == SignalKind::Event)
                    .collect();
                if audio_inputs != 1 || event_outputs.len() != 1 {
                    report.error(
                        "invalid_silence_detector_ports",
                        "silence detector requires one Audio input and one Event output",
                    );
                } else if !["audio.silence.started", "audio.silence.resumed"]
                    .iter()
                    .all(|kind| {
                        event_outputs[0]
                            .event_types
                            .iter()
                            .any(|found| found == kind)
                    })
                {
                    report.error(
                        "invalid_silence_detector_events",
                        "silence detector output must declare started and resumed events",
                    );
                }
            }
            crate::audio::graph::NodeProperties::Action { config } => {
                if let Err(message) = config.validate() {
                    report.error("invalid_action_config", message);
                }
                validate_event_target_ports(node, &mut report, "Action");
            }
            crate::audio::graph::NodeProperties::LogOutput { config } => {
                if let Err(message) = config.validate() {
                    report.error("invalid_log_output_config", message);
                }
                validate_event_target_ports(node, &mut report, "Log Output");
            }
            _ => {}
        }
    }

    for cycle in crate::routing::loop_detector::explicit_cycles(graph) {
        report.warning(
            "explicit_graph_loop",
            format!(
                "explicit graph loop detected: {}",
                cycle
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ),
        );
    }

    for edge in &graph.event_edges {
        if event_path_exists(graph, edge.to_node, edge.from_node, Some(edge.id)) {
            report.error(
                "event_graph_loop",
                format!("event edge {} creates an Event Graph cycle", edge.id),
            );
        }
    }

    report
}

fn validate_event_target_ports(
    node: &crate::audio::graph::NodeSpec,
    report: &mut ValidationReport,
    label: &str,
) {
    let event_inputs = node
        .input_ports
        .iter()
        .filter(|port| port.signal_kind == SignalKind::Event)
        .count();
    let has_audio_port = node
        .input_ports
        .iter()
        .chain(&node.output_ports)
        .any(|port| port.signal_kind == SignalKind::Audio);
    if event_inputs != 1 || has_audio_port || !node.output_ports.is_empty() {
        report.error(
            "invalid_event_target_ports",
            format!("{label} requires one Event input and no Audio/output ports"),
        );
    }
}

fn event_path_exists(
    graph: &GraphSnapshot,
    start: NodeId,
    target: NodeId,
    ignored_edge: Option<crate::audio::graph::EdgeId>,
) -> bool {
    let mut pending = vec![start];
    let mut visited = HashSet::new();
    while let Some(node) = pending.pop() {
        if node == target {
            return true;
        }
        if !visited.insert(node) {
            continue;
        }
        pending.extend(
            graph
                .event_edges
                .iter()
                .filter(|edge| edge.from_node == node && Some(edge.id) != ignored_edge)
                .map(|edge| edge.to_node),
        );
    }
    false
}
