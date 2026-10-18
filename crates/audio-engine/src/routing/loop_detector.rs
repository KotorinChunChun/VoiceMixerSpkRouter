use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::audio::graph::{GraphSnapshot, NodeId, NodeProperties};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoopWarningKind {
    ExplicitGraph,
    SameEndpoint,
    ImplicitDeviceRoute,
    OsListenRoute,
    PossibleExternal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct LoopWarning {
    pub kind: LoopWarningKind,
    pub message: String,
    pub nodes: Vec<NodeId>,
}

pub fn explicit_cycles(graph: &GraphSnapshot) -> Vec<Vec<NodeId>> {
    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for edge in &graph.edges {
        adjacency
            .entry(edge.from_node)
            .or_default()
            .push(edge.to_node);
    }
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut on_stack = HashSet::new();
    let mut stack = Vec::<NodeId>::new();

    fn dfs(
        n: NodeId,
        adjacency: &HashMap<NodeId, Vec<NodeId>>,
        visited: &mut HashSet<NodeId>,
        on_stack: &mut HashSet<NodeId>,
        stack: &mut Vec<NodeId>,
        result: &mut Vec<Vec<NodeId>>,
    ) {
        if visited.contains(&n) {
            return;
        }
        visited.insert(n);
        on_stack.insert(n);
        stack.push(n);
        if let Some(nexts) = adjacency.get(&n) {
            for &m in nexts {
                if !visited.contains(&m) {
                    dfs(m, adjacency, visited, on_stack, stack, result);
                } else if on_stack.contains(&m) {
                    if let Some(pos) = stack.iter().position(|x| *x == m) {
                        let mut cycle = stack[pos..].to_vec();
                        cycle.push(m);
                        result.push(cycle);
                    }
                }
            }
        }
        stack.pop();
        on_stack.remove(&n);
    }

    for &node_id in graph.nodes.keys() {
        dfs(
            node_id,
            &adjacency,
            &mut visited,
            &mut on_stack,
            &mut stack,
            &mut result,
        );
    }
    result
}

pub fn detect_loop_warnings(graph: &GraphSnapshot) -> Vec<LoopWarning> {
    let mut out = Vec::new();
    for cycle in explicit_cycles(graph) {
        out.push(LoopWarning {
            kind: LoopWarningKind::ExplicitGraph,
            message: "Explicit Audio Graph cycle detected. Runtime uses a one-block feedback delay; this can create acoustic feedback.".into(),
            nodes: cycle,
        });
    }

    let mut captures: Vec<(&str, NodeId)> = Vec::new();
    let mut renders: Vec<(&str, NodeId)> = Vec::new();
    for node in graph.nodes.values() {
        match &node.properties {
            NodeProperties::EndpointLoopback { endpoint_id, .. } => {
                captures.push((endpoint_id, node.id))
            }
            NodeProperties::Monitor { endpoint_id, .. }
            | NodeProperties::VirtualDevice { endpoint_id, .. } => {
                renders.push((endpoint_id, node.id))
            }
            _ => {}
        }
    }
    for (capture_id, capture_node) in &captures {
        for (render_id, render_node) in &renders {
            if capture_id == render_id {
                out.push(LoopWarning {
                    kind: LoopWarningKind::SameEndpoint,
                    message: format!("Render endpoint '{}' is also captured by endpoint loopback; feedback is possible.", capture_id),
                    nodes: vec![*capture_node, *render_node],
                });
            }
        }
    }
    out
}

/// Extends graph-only loop analysis with OS/driver routes that live outside the
/// explicit graph. Warnings are advisory by product requirement; routes are
/// never force-disconnected by this detector.
pub fn detect_loop_warnings_with_implicit(
    graph: &GraphSnapshot,
    implicit_routes: &[crate::windows::implicit_routes::ImplicitRoute],
) -> Vec<LoopWarning> {
    use crate::windows::implicit_routes::ImplicitRouteKind;

    let mut out = detect_loop_warnings(graph);
    let mut capture_nodes: HashMap<&str, Vec<NodeId>> = HashMap::new();
    let mut render_nodes: HashMap<&str, Vec<NodeId>> = HashMap::new();

    for node in graph.nodes.values() {
        match &node.properties {
            NodeProperties::Microphone { endpoint_id, .. } => {
                capture_nodes.entry(endpoint_id).or_default().push(node.id);
            }
            NodeProperties::EndpointLoopback { endpoint_id, .. } => {
                // Endpoint loopback identifies a render endpoint being captured.
                capture_nodes.entry(endpoint_id).or_default().push(node.id);
            }
            NodeProperties::Monitor { endpoint_id, .. }
            | NodeProperties::VirtualDevice { endpoint_id, .. } => {
                render_nodes.entry(endpoint_id).or_default().push(node.id);
            }
            _ => {}
        }
    }

    for route in implicit_routes {
        match route.kind {
            ImplicitRouteKind::VirtualDevicePair => {
                // Driver route: graph render endpoint -> virtual capture endpoint.
                if let (Some(writers), Some(readers)) = (
                    render_nodes.get(route.from.as_str()),
                    capture_nodes.get(route.to.as_str()),
                ) {
                    let mut nodes = writers.clone();
                    nodes.extend(readers.iter().copied());
                    out.push(LoopWarning {
                        kind: LoopWarningKind::ImplicitDeviceRoute,
                        message: format!(
                            "Virtual-device route '{}' -> '{}' is both written and captured by this graph; feedback is possible.",
                            route.from, route.to
                        ),
                        nodes,
                    });
                }
            }
            ImplicitRouteKind::OsListenToDevice => {
                // OS route: capture endpoint -> render endpoint. If that render
                // endpoint is also loopback-captured by the graph, the route can
                // feed the same source back into the mixer.
                if let (Some(source_nodes), Some(render_capture_nodes)) = (
                    capture_nodes.get(route.from.as_str()),
                    capture_nodes.get(route.to.as_str()),
                ) {
                    let mut nodes = source_nodes.clone();
                    nodes.extend(render_capture_nodes.iter().copied());
                    out.push(LoopWarning {
                        kind: LoopWarningKind::OsListenRoute,
                        message: format!(
                            "Windows 'Listen to this device' routes '{}' -> '{}' and the destination is captured by the graph; feedback is possible.",
                            route.from, route.to
                        ),
                        nodes,
                    });
                }
            }
            ImplicitRouteKind::ExternalApplication => {
                out.push(LoopWarning {
                    kind: LoopWarningKind::PossibleExternal,
                    message: route.note.clone(),
                    nodes: Vec::new(),
                });
            }
        }
    }
    out
}
