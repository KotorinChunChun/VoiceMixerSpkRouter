use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use crate::{
    audio::graph::{EdgeId, GraphSnapshot, NodeId},
    error::{BackendError, Result},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RouteSubgraph {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
}

pub fn upstream_nodes(graph: &GraphSnapshot, node_id: NodeId) -> Result<Vec<NodeId>> {
    ensure_node(graph, node_id)?;
    Ok(walk(graph, node_id, true))
}

pub fn downstream_nodes(graph: &GraphSnapshot, node_id: NodeId) -> Result<Vec<NodeId>> {
    ensure_node(graph, node_id)?;
    Ok(walk(graph, node_id, false))
}

pub fn related_route_nodes(graph: &GraphSnapshot, node_id: NodeId) -> Result<Vec<NodeId>> {
    ensure_node(graph, node_id)?;
    let mut related = BTreeSet::from([node_id]);
    related.extend(walk(graph, node_id, true));
    related.extend(walk(graph, node_id, false));
    Ok(related.into_iter().collect())
}

pub fn route_to_output(graph: &GraphSnapshot, output_node: NodeId) -> Result<RouteSubgraph> {
    let output = graph
        .nodes
        .get(&output_node)
        .ok_or_else(|| BackendError::NodeNotFound(output_node.to_string()))?;
    if output.node_type.class() != crate::audio::graph::NodeClass::Output {
        return Err(BackendError::InvalidOperation(
            "route target is not an output node".into(),
        ));
    }
    let mut nodes = BTreeSet::from([output_node]);
    nodes.extend(walk(graph, output_node, true));
    let mut edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|edge| nodes.contains(&edge.from_node) && nodes.contains(&edge.to_node))
        .map(|edge| edge.id)
        .collect();
    edges.sort();
    Ok(RouteSubgraph {
        nodes: nodes.into_iter().collect(),
        edges,
    })
}

pub fn path_exists(graph: &GraphSnapshot, from: NodeId, to: NodeId) -> bool {
    from == to || walk(graph, from, false).contains(&to)
}

fn walk(graph: &GraphSnapshot, start: NodeId, upstream: bool) -> Vec<NodeId> {
    let mut visited = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(current) = stack.pop() {
        let next = graph.edges.iter().filter_map(|edge| {
            if upstream && edge.to_node == current {
                Some(edge.from_node)
            } else if !upstream && edge.from_node == current {
                Some(edge.to_node)
            } else {
                None
            }
        });
        for node in next {
            if visited.insert(node) {
                stack.push(node);
            }
        }
    }
    visited.remove(&start);
    let mut result: Vec<_> = visited.into_iter().collect();
    result.sort();
    result
}

fn ensure_node(graph: &GraphSnapshot, node_id: NodeId) -> Result<()> {
    graph
        .nodes
        .contains_key(&node_id)
        .then_some(())
        .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))
}
