use crate::{
    audio::graph::NodeProperties,
    config::model::PersistedConfiguration,
    error::{BackendError, Result},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}
impl ConfigStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn base_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }
    pub fn resolve_relative(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir().join(path)
        }
    }
    pub fn load(&self) -> Result<PersistedConfiguration> {
        crate::config::transaction::recover(&self.base_dir())?;
        if !self.path.exists() {
            return Ok(PersistedConfiguration::default());
        }
        let bytes = fs::read(&self.path).map_err(|source| BackendError::ConfigIo {
            path: self.path.clone(),
            source,
        })?;
        let mut config: PersistedConfiguration = serde_json::from_slice(&bytes)?;
        for node in config.nodes.values_mut() {
            crate::plugins::migrate_legacy_rvc_plugin_id(&mut node.properties);
            migrate_legacy_sample_audio(self, &mut node.properties);
        }
        for node in config.registered_nodes.values_mut() {
            crate::plugins::migrate_legacy_rvc_plugin_id(&mut node.properties);
            migrate_legacy_sample_audio(self, &mut node.properties);
        }
        let migrated_virtual_devices =
            crate::config::routing_profile::normalize_legacy_virtual_devices(
                &mut config.nodes,
                &mut config.registered_nodes,
            );
        let migrated_ducking =
            crate::config::routing_profile::has_legacy_multi_input_ducking(config.nodes.values());
        crate::config::routing_profile::migrate_legacy_multi_input_ducking(
            "現在の設定",
            &mut config.nodes,
            &mut config.routing_edges,
            &config.event_edges,
            &mut config.node_layouts,
        )?;
        crate::config::routing_profile::reject_unplaced_legacy_ducking(
            "現在の設定",
            &mut config.registered_nodes,
            &config.nodes,
        )?;
        if migrated_ducking || migrated_virtual_devices {
            self.save(&config)?;
        }
        Ok(config)
    }
    pub fn save(&self, config: &PersistedConfiguration) -> Result<()> {
        let mut config = config.clone();
        super::routing_profile::normalize_legacy_virtual_devices(
            &mut config.nodes,
            &mut config.registered_nodes,
        );
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        super::transaction::commit_single(
            &self.base_dir(),
            &self.path,
            serde_json::to_vec_pretty(&config)?,
        )
    }
}

fn migrate_legacy_sample_audio(store: &ConfigStore, properties: &mut NodeProperties) {
    let NodeProperties::FileInput { path, .. } = properties else {
        return;
    };
    let normalized = path.to_string_lossy().replace('\\', "/");
    if matches!(
        normalized.as_str(),
        "samples/example.wav" | "sample/example.wav"
    ) {
        *path = crate::config::routing_profile::sample_audio_path(store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        audio::graph::{NodeProperties, NodeSpec, PluginInstanceId, TransformFailurePolicy},
        plugins::{LEGACY_RVC_KIKI_PLUGIN_ID, RVC_CUDA_PLUGIN_ID},
        registry::node_registry::RegisteredNode,
    };
    use std::collections::BTreeMap;

    fn legacy_properties() -> NodeProperties {
        let mut parameters = BTreeMap::new();
        parameters.insert("model".into(), "models/kikiV1.pth".into());
        parameters.insert("index".into(), "models/kikiV1.index".into());
        NodeProperties::TransformPlugin {
            plugin_id: LEGACY_RVC_KIKI_PLUGIN_ID.into(),
            instance_id: PluginInstanceId::new(),
            parameters,
            bypass: false,
            failure_policy: TransformFailurePolicy::Silence,
            switch_policy: crate::audio::graph::SwitchPolicy::Silence,
            gain_db: 0.0,
            mute: false,
        }
    }

    #[test]
    fn load_migrates_legacy_rvc_plugin_ids() {
        let temp = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(temp.path().join("voicemixer.json"));
        let graph_node = NodeSpec::new(
            crate::audio::graph::NodeType::TransformPlugin,
            "Legacy RVC",
            legacy_properties(),
        );
        let registered = RegisteredNode::user("Legacy RVC", legacy_properties());
        let mut config = PersistedConfiguration::default();
        config.nodes.insert(graph_node.id, graph_node);
        config.registered_nodes.insert(registered.id, registered);
        store.save(&config).unwrap();

        let loaded = store.load().unwrap();
        for properties in loaded.nodes.values().map(|node| &node.properties).chain(
            loaded
                .registered_nodes
                .values()
                .map(|node| &node.properties),
        ) {
            let NodeProperties::TransformPlugin {
                plugin_id,
                parameters,
                ..
            } = properties
            else {
                panic!("expected transform plugin")
            };
            assert_eq!(plugin_id, RVC_CUDA_PLUGIN_ID);
            assert_eq!(parameters["model"], "models/KahoA.pth");
            assert_eq!(parameters["index"], "models/Kaho.index");
        }
    }

    #[test]
    fn load_migrates_legacy_sample_audio_path() {
        let temp = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(temp.path().join("voicemixer.json"));
        let graph_node = NodeSpec::new(
            crate::audio::graph::NodeType::FileInput,
            "Legacy sample",
            NodeProperties::FileInput {
                path: "samples/example.wav".into(),
                looped: false,
                loop_wait_ms: 0,
                gain_db: 0.0,
                mute: false,
            },
        );
        let registered = RegisteredNode::user(
            "Legacy sample",
            NodeProperties::FileInput {
                path: "samples/example.wav".into(),
                looped: false,
                loop_wait_ms: 0,
                gain_db: 0.0,
                mute: false,
            },
        );
        let mut config = PersistedConfiguration::default();
        config.nodes.insert(graph_node.id, graph_node);
        config.registered_nodes.insert(registered.id, registered);
        store.save(&config).unwrap();

        let loaded = store.load().unwrap();
        let expected = crate::config::routing_profile::sample_audio_path(&store);
        for properties in loaded.nodes.values().map(|node| &node.properties).chain(
            loaded
                .registered_nodes
                .values()
                .map(|node| &node.properties),
        ) {
            let NodeProperties::FileInput { path, .. } = properties else {
                panic!("expected file input")
            };
            assert_eq!(path, &expected);
        }
    }
}
