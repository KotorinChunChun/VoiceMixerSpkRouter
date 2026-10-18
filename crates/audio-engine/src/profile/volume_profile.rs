use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    audio::graph::{EdgeId, GraphSnapshot, NodeId, NodeProperties, PortId},
    error::{BackendError, Result},
};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct VolumeProfileId(pub Uuid);

impl VolumeProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for VolumeProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for VolumeProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GainMuteState {
    pub gain_db: f32,
    pub mute: bool,
}

impl GainMuteState {
    fn validate(self, target: &str) -> Result<()> {
        if !self.gain_db.is_finite() {
            return Err(BackendError::InvalidOperation(format!(
                "{target} gain must be finite"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VolumeMuteProfile {
    pub profile_id: VolumeProfileId,
    pub display_name: String,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    #[serde(default)]
    pub node_states: HashMap<NodeId, GainMuteState>,
    #[serde(default)]
    pub port_states: HashMap<PortId, GainMuteState>,
    #[serde(default)]
    pub edge_states: HashMap<EdgeId, GainMuteState>,
}

impl VolumeMuteProfile {
    pub fn capture(
        graph: &GraphSnapshot,
        profile_id: VolumeProfileId,
        display_name: impl Into<String>,
        created_unix_ms: u64,
        updated_unix_ms: u64,
    ) -> Result<Self> {
        let display_name = display_name.into();
        if display_name.trim().is_empty() {
            return Err(BackendError::InvalidOperation(
                "volume profile display name cannot be empty".into(),
            ));
        }
        let mut node_states = HashMap::new();
        let mut port_states = HashMap::new();
        for node in graph.nodes.values() {
            if let Some((gain_db, mute)) = node.properties.gain_mute() {
                node_states.insert(node.id, GainMuteState { gain_db, mute });
            }
            if let NodeProperties::MultiInput { slots, .. } = &node.properties {
                for slot in slots {
                    port_states.insert(
                        slot.port_id,
                        GainMuteState {
                            gain_db: slot.gain_db,
                            mute: slot.mute,
                        },
                    );
                }
            }
        }
        let edge_states = graph
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.id,
                    GainMuteState {
                        gain_db: edge.gain_db,
                        mute: edge.mute,
                    },
                )
            })
            .collect();
        Ok(Self {
            profile_id,
            display_name,
            created_unix_ms,
            updated_unix_ms,
            node_states,
            port_states,
            edge_states,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.display_name.trim().is_empty() {
            return Err(BackendError::InvalidOperation(
                "volume profile display name cannot be empty".into(),
            ));
        }
        for state in self.node_states.values().copied() {
            state.validate("node")?;
        }
        for state in self.port_states.values().copied() {
            state.validate("port")?;
        }
        for state in self.edge_states.values().copied() {
            state.validate("edge")?;
        }
        Ok(())
    }

    pub fn apply_to(&self, graph: &mut GraphSnapshot) -> Result<ApplyVolumeProfileReport> {
        self.validate()?;
        let mut report = ApplyVolumeProfileReport {
            profile_id: self.profile_id,
            ..Default::default()
        };

        for (node_id, state) in &self.node_states {
            match graph.nodes.get_mut(node_id) {
                Some(node) => {
                    if node.properties.set_gain_db(state.gain_db) {
                        node.properties.set_mute(state.mute);
                        report.updated_nodes += 1;
                    } else {
                        report.missing_node_ids.push(*node_id);
                    }
                }
                None => report.missing_node_ids.push(*node_id),
            }
        }

        for (edge_id, state) in &self.edge_states {
            let Some(edge) = graph.edges.iter_mut().find(|edge| edge.id == *edge_id) else {
                report.missing_edge_ids.push(*edge_id);
                continue;
            };
            edge.gain_db = state.gain_db;
            edge.mute = state.mute;
            let destination = edge.to_node;
            let port = edge.to_port;
            if let Some(NodeProperties::MultiInput { slots, .. }) = graph
                .nodes
                .get_mut(&destination)
                .map(|node| &mut node.properties)
            {
                if let Some(slot) = slots.iter_mut().find(|slot| slot.port_id == port) {
                    slot.gain_db = state.gain_db;
                    slot.mute = state.mute;
                }
            }
            report.updated_edges += 1;
        }

        for (port_id, state) in &self.port_states {
            let slot = graph.nodes.values_mut().find_map(|node| {
                let NodeProperties::MultiInput { slots, .. } = &mut node.properties else {
                    return None;
                };
                slots.iter_mut().find(|slot| slot.port_id == *port_id)
            });
            match slot {
                Some(slot) => {
                    slot.gain_db = state.gain_db;
                    slot.mute = state.mute;
                    report.updated_ports += 1;
                }
                None => report.missing_port_ids.push(*port_id),
            }
        }

        report.missing_node_ids.sort();
        report.missing_port_ids.sort();
        report.missing_edge_ids.sort();
        Ok(report)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplyVolumeProfileReport {
    pub profile_id: VolumeProfileId,
    pub updated_nodes: usize,
    pub updated_ports: usize,
    pub updated_edges: usize,
    #[serde(default)]
    pub missing_node_ids: Vec<NodeId>,
    #[serde(default)]
    pub missing_port_ids: Vec<PortId>,
    #[serde(default)]
    pub missing_edge_ids: Vec<EdgeId>,
    pub replays_stopped: usize,
}
