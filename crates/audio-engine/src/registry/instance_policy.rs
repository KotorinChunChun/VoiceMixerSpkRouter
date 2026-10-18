use serde::{Deserialize, Serialize};

use crate::audio::graph::{NodeProperties, NodeType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstancePolicy {
    RegisteredInstanceOnly,
    UnlimitedInstances,
}

impl InstancePolicy {
    pub fn for_node_type(node_type: NodeType) -> Self {
        if matches!(
            node_type,
            NodeType::MultiInput | NodeType::AudioProcessingGroup
        ) {
            Self::UnlimitedInstances
        } else {
            Self::RegisteredInstanceOnly
        }
    }

    /// Migrates the persisted policy for built-in DSP registrations. Older
    /// profiles stored these as singleton registrations, but built-in DSP
    /// nodes are now repeatable. External plugin policies remain unchanged.
    pub fn migrate_persisted(properties: &NodeProperties, current: Self) -> Self {
        match properties {
            NodeProperties::TransformPlugin { plugin_id, .. }
                if plugin_id.starts_with("audio-router.builtin.") =>
            {
                Self::UnlimitedInstances
            }
            NodeProperties::AudioProcessingGroup { .. } => Self::UnlimitedInstances,
            _ => current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn built_in_transform_policy_migrates_without_changing_external_plugins() {
        let built_in = NodeProperties::TransformPlugin {
            plugin_id: "audio-router.builtin.compressor".into(),
            instance_id: crate::audio::graph::PluginInstanceId::new(),
            parameters: BTreeMap::new(),
            bypass: false,
            failure_policy: Default::default(),
            switch_policy: Default::default(),
            gain_db: 0.0,
            mute: false,
        };
        let external = NodeProperties::TransformPlugin {
            plugin_id: "vendor.plugin".into(),
            instance_id: crate::audio::graph::PluginInstanceId::new(),
            parameters: BTreeMap::new(),
            bypass: false,
            failure_policy: Default::default(),
            switch_policy: Default::default(),
            gain_db: 0.0,
            mute: false,
        };
        assert_eq!(
            InstancePolicy::migrate_persisted(&built_in, InstancePolicy::RegisteredInstanceOnly),
            InstancePolicy::UnlimitedInstances
        );
        assert_eq!(
            InstancePolicy::migrate_persisted(&external, InstancePolicy::RegisteredInstanceOnly),
            InstancePolicy::RegisteredInstanceOnly
        );
        assert_eq!(
            InstancePolicy::for_node_type(NodeType::AudioProcessingGroup),
            InstancePolicy::UnlimitedInstances
        );
    }
}

impl Default for InstancePolicy {
    fn default() -> Self {
        Self::RegisteredInstanceOnly
    }
}
