//! 配線の挿入／削除案を複製Graph上で構築する。呼出元が保存と実行系の交換を行う。
use std::collections::HashSet;

use crate::audio::graph::{Edge, EdgeId, GraphSnapshot, NodeClass, NodeId, SignalKind};
use crate::error::{BackendError, Result};
use crate::routing::validator::{validate_graph, ValidationSeverity};

fn finish(mut graph: GraphSnapshot) -> Result<GraphSnapshot> {
    if graph
        .edges
        .iter()
        .any(|edge| edge.from_node == edge.to_node)
    {
        return Err(BackendError::GraphValidation(
            "自己接続は作成できません".into(),
        ));
    }
    let report = validate_graph(&graph);
    let errors: Vec<_> = report
        .issues
        .iter()
        .filter(|issue| {
            issue.severity == ValidationSeverity::Error
                && issue.code != "required_plugin_input_unconnected"
        })
        .collect();
    if !errors.is_empty() {
        return Err(BackendError::GraphValidation(
            errors
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    graph.revision = graph.revision.saturating_add(1);
    Ok(graph)
}

pub fn insert_transform(
    graph: &GraphSnapshot,
    id: NodeId,
    edge_id: EdgeId,
) -> Result<GraphSnapshot> {
    let node = graph
        .nodes
        .get(&id)
        .ok_or_else(|| BackendError::NodeNotFound(id.to_string()))?;
    if node.node_type.class() != NodeClass::Transform
        || graph
            .edges
            .iter()
            .any(|edge| edge.from_node == id || edge.to_node == id)
        || graph
            .event_edges
            .iter()
            .any(|edge| edge.from_node == id || edge.to_node == id)
    {
        return Err(BackendError::InvalidOperation(
            "未接続の変換ノードだけを挿入できます".into(),
        ));
    }
    let input = node
        .input_ports
        .iter()
        .find(|port| port.signal_kind == SignalKind::Audio)
        .ok_or_else(|| BackendError::InvalidOperation("音声入力端子がありません".into()))?;
    let output = node
        .output_ports
        .iter()
        .find(|port| port.signal_kind == SignalKind::Audio)
        .ok_or_else(|| BackendError::InvalidOperation("音声出力端子がありません".into()))?;
    let original = graph
        .edges
        .iter()
        .find(|edge| edge.id == edge_id)
        .ok_or_else(|| BackendError::EdgeNotFound(edge_id.to_string()))?;
    let mut proposed = graph.clone();
    proposed.edges.retain(|edge| edge.id != edge_id);
    let mut entry = original.clone();
    entry.to_node = id;
    entry.to_port = input.id;
    let exit = Edge::new(id, output.id, original.to_node, original.to_port);
    proposed.edges.extend([entry, exit]);
    finish(proposed)
}

/// 削除対象の第一接続済み音声入力を辿り、生き残る上流までを一つの辺へ畳む。
fn bypass_source(
    graph: &GraphSnapshot,
    removed: &HashSet<NodeId>,
    exit: &Edge,
    visited: &mut HashSet<NodeId>,
) -> Result<Option<Edge>> {
    if !removed.contains(&exit.from_node) {
        return Ok(Some(exit.clone()));
    }
    if !visited.insert(exit.from_node) {
        return Err(BackendError::GraphValidation(
            "削除対象内の循環は迂回できません".into(),
        ));
    }
    let node = &graph.nodes[&exit.from_node];
    if node.node_type.class() != NodeClass::Transform {
        return Ok(None);
    }
    let upstream = node
        .input_ports
        .iter()
        .filter(|port| port.signal_kind == SignalKind::Audio)
        .find_map(|port| {
            graph
                .incoming(node.id)
                .filter(|edge| edge.to_port == port.id)
                .min_by_key(|edge| edge.id)
        });
    let Some(upstream) = upstream else {
        return Ok(None);
    };
    let Some(mut merged) = bypass_source(graph, removed, upstream, visited)? else {
        return Ok(None);
    };
    merged.id = exit.id;
    merged.to_node = exit.to_node;
    merged.to_port = exit.to_port;
    merged.gain_db += exit.gain_db;
    merged.mute |= exit.mute;
    merged.color = exit.color.clone().or(merged.color);
    Ok(Some(merged))
}

pub fn remove_with_bypass(graph: &GraphSnapshot, ids: &[NodeId]) -> Result<GraphSnapshot> {
    let removed: HashSet<_> = ids.iter().copied().collect();
    for id in &removed {
        if !graph.nodes.contains_key(id) {
            return Err(BackendError::NodeNotFound(id.to_string()));
        }
    }
    if removed.is_empty() {
        return Ok(graph.clone());
    }
    let mut proposed = graph.clone();
    proposed.edges.clear();
    for edge in &graph.edges {
        if removed.contains(&edge.to_node) {
            continue;
        }
        if let Some(edge) = bypass_source(graph, &removed, edge, &mut HashSet::new())? {
            proposed.edges.push(edge);
        }
    }
    proposed.nodes.retain(|id, _| !removed.contains(id));
    proposed.layouts.retain(|id, _| !removed.contains(id));
    proposed
        .event_edges
        .retain(|edge| !removed.contains(&edge.from_node) && !removed.contains(&edge.to_node));
    finish(proposed)
}
