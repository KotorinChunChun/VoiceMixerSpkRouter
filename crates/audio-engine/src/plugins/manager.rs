use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use parking_lot::RwLock;
use serde_json::Value;

use crate::{
    audio::graph::{
        NodeProperties, NodeSpec, NodeType, PluginInstanceId, PortDirection, PortSpec,
        SwitchPolicy, TransformFailurePolicy,
    },
    error::{BackendError, Result},
    plugins::{
        instance::{PluginPrepareContext, PluginProcessor, TransformInstance},
        manifest::{ParameterDescriptor, ParameterKind, PluginDescriptor, PluginExecutionMode},
    },
};

pub trait PluginFactory: Send + Sync {
    fn descriptor(&self) -> PluginDescriptor;
    fn create(&self, parameters: &BTreeMap<String, Value>) -> Result<Box<dyn TransformInstance>>;
}

#[derive(Default)]
pub struct PluginManager {
    factories: RwLock<HashMap<String, Arc<dyn PluginFactory>>>,
    instances: RwLock<HashMap<PluginInstanceId, Arc<PluginProcessor>>>,
}

impl PluginManager {
    pub fn register_factory(&self, factory: Arc<dyn PluginFactory>) -> Result<()> {
        let descriptor = factory.descriptor();
        descriptor.validate().map_err(BackendError::Plugin)?;
        // API v1 keeps the one-in/one-out process contract. API v2 may expose
        // up to eight concrete ports and is dispatched through `process_ports`.
        if descriptor.api_version == crate::plugins::manifest::TRANSFORM_PLUGIN_API_VERSION
            && (descriptor.input_ports.len() != 1 || descriptor.output_ports.len() != 1)
        {
            return Err(BackendError::Plugin(format!(
                "plugin '{}' declares {} input / {} output ports; transform plugin API v1 requires exactly 1 input and 1 output",
                descriptor.plugin_id, descriptor.input_ports.len(), descriptor.output_ports.len()
            )));
        }
        if descriptor
            .input_ports
            .iter()
            .any(|p| p.direction != PortDirection::Input)
            || descriptor
                .output_ports
                .iter()
                .any(|p| p.direction != PortDirection::Output)
        {
            return Err(BackendError::Plugin(format!(
                "plugin '{}' has invalid port directions",
                descriptor.plugin_id
            )));
        }
        let mut port_ids = std::collections::HashSet::new();
        if descriptor
            .input_ports
            .iter()
            .chain(descriptor.output_ports.iter())
            .any(|p| !port_ids.insert(p.id.clone()))
        {
            return Err(BackendError::Plugin(format!(
                "plugin '{}' contains duplicate port ids",
                descriptor.plugin_id
            )));
        }
        let mut parameter_ids = std::collections::HashSet::new();
        if descriptor
            .parameters
            .iter()
            .any(|p| !parameter_ids.insert(p.id.clone()))
        {
            return Err(BackendError::Plugin(format!(
                "plugin '{}' contains duplicate parameter ids",
                descriptor.plugin_id
            )));
        }
        let mut factories = self.factories.write();
        if factories.contains_key(&descriptor.plugin_id) {
            return Err(BackendError::Plugin(format!(
                "duplicate plugin_id '{}'",
                descriptor.plugin_id
            )));
        }
        factories.insert(descriptor.plugin_id, factory);
        Ok(())
    }

    pub fn list_plugins(&self) -> Vec<PluginDescriptor> {
        let mut items: Vec<_> = self
            .factories
            .read()
            .values()
            .map(|f| f.descriptor())
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    pub fn descriptor(&self, plugin_id: &str) -> Result<PluginDescriptor> {
        self.factories
            .read()
            .get(plugin_id)
            .map(|f| f.descriptor())
            .ok_or_else(|| {
                BackendError::Plugin(format!("transform plugin '{plugin_id}' not found"))
            })
    }

    pub fn validate_parameter(&self, plugin_id: &str, id: &str, value: &Value) -> Result<()> {
        let descriptor = self.descriptor(plugin_id)?;
        let parameter = descriptor
            .parameters
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| {
                BackendError::Plugin(format!("plugin '{plugin_id}' has no parameter '{id}'"))
            })?;
        validate_parameter_value(plugin_id, parameter, value)
    }

    pub fn validate_parameters(
        &self,
        plugin_id: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<()> {
        for (id, value) in parameters {
            self.validate_parameter(plugin_id, id, value)?;
        }
        Ok(())
    }

    pub fn create_instance(
        &self,
        id: PluginInstanceId,
        plugin_id: &str,
        parameters: BTreeMap<String, Value>,
        prepare: PluginPrepareContext,
    ) -> Result<Arc<PluginProcessor>> {
        if let Some(existing) = self.instances.read().get(&id).cloned() {
            return Ok(existing);
        }
        self.validate_parameters(plugin_id, &parameters)?;
        let factory = self
            .factories
            .read()
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| {
                BackendError::Plugin(format!("transform plugin '{plugin_id}' not found"))
            })?;
        let descriptor = factory.descriptor();
        let manifest_latency_frames = descriptor.reported_latency_frames;
        let instance = factory.create(&parameters)?;
        let processor = PluginProcessor::new_with_manifest_latency(
            instance,
            parameters,
            manifest_latency_frames,
        );
        processor.set_realtime_busy_bypass(matches!(
            descriptor.execution_mode,
            PluginExecutionMode::BuiltIn
        ));
        processor.prepare(prepare)?;
        self.instances.write().insert(id, processor.clone());
        Ok(processor)
    }

    pub fn recreate_instance(
        &self,
        id: PluginInstanceId,
        plugin_id: &str,
        parameters: BTreeMap<String, Value>,
        prepare: PluginPrepareContext,
    ) -> Result<Arc<PluginProcessor>> {
        let processor = self.get(id)?;
        self.validate_parameters(plugin_id, &parameters)?;
        let factory = self
            .factories
            .read()
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| {
                BackendError::Plugin(format!("transform plugin '{plugin_id}' not found"))
            })?;
        processor.begin_switch();
        let result = (|| {
            let instance = factory.create(&parameters)?;
            processor.replace_instance(instance, parameters, prepare)
        })();
        processor.end_switch();
        result?;
        Ok(processor)
    }

    pub fn get(&self, id: PluginInstanceId) -> Result<Arc<PluginProcessor>> {
        self.instances
            .read()
            .get(&id)
            .cloned()
            .ok_or_else(|| BackendError::Plugin(format!("plugin instance {id} not found")))
    }

    pub fn remove(&self, id: PluginInstanceId) {
        self.instances.write().remove(&id);
    }

    pub fn list_instances(&self) -> Vec<PluginInstanceId> {
        self.instances.read().keys().copied().collect()
    }
    /// Creates a graph node whose concrete ports are derived from the plugin
    /// manifest. This is the preferred path for all new transform nodes.
    pub fn build_node_spec(
        &self,
        plugin_id: &str,
        display_name: impl Into<String>,
        mut parameters: BTreeMap<String, Value>,
    ) -> Result<NodeSpec> {
        let descriptor = self.descriptor(plugin_id)?;
        let defaults = descriptor.default_parameters();
        for (key, value) in defaults {
            parameters.entry(key).or_insert(value);
        }
        self.validate_parameters(plugin_id, &parameters)?;

        let node = NodeSpec {
            id: crate::audio::graph::NodeId::new(),
            registered_node_id: None,
            node_type: NodeType::TransformPlugin,
            display_name: display_name.into(),
            enabled: true,
            runtime_enabled: true,
            state: crate::audio::graph::RuntimeState::Stopped,
            comment: String::new(),
            show_in_playback: true,
            playback_order: 0,
            input_ports: descriptor
                .input_ports
                .iter()
                .map(|port| {
                    let mut spec = PortSpec::plugin(
                        port.id.clone(),
                        port.name.clone(),
                        PortDirection::Input,
                        port.max_connections,
                    );
                    spec.required = port.required;
                    spec
                })
                .collect(),
            output_ports: descriptor
                .output_ports
                .iter()
                .map(|port| {
                    let mut spec = PortSpec::plugin(
                        port.id.clone(),
                        port.name.clone(),
                        PortDirection::Output,
                        port.max_connections,
                    );
                    spec.required = port.required;
                    spec
                })
                .collect(),
            properties: NodeProperties::TransformPlugin {
                plugin_id: plugin_id.to_string(),
                instance_id: PluginInstanceId::new(),
                parameters,
                bypass: false,
                failure_policy: TransformFailurePolicy::Silence,
                switch_policy: SwitchPolicy::Silence,
                gain_db: 0.0,
                mute: false,
            },
        };
        Ok(node)
    }
}

fn validate_parameter_value(plugin_id: &str, p: &ParameterDescriptor, value: &Value) -> Result<()> {
    let numeric = match p.kind {
        ParameterKind::Boolean if !value.is_boolean() => {
            return Err(type_error(plugin_id, p, "boolean"))
        }
        ParameterKind::Integer => {
            let n = value
                .as_i64()
                .map(|x| x as f64)
                .or_else(|| value.as_u64().map(|x| x as f64))
                .ok_or_else(|| type_error(plugin_id, p, "integer"))?;
            Some(n)
        }
        ParameterKind::Number => Some(
            value
                .as_f64()
                .ok_or_else(|| type_error(plugin_id, p, "number"))?,
        ),
        ParameterKind::String | ParameterKind::File if !value.is_string() => {
            return Err(type_error(plugin_id, p, "string"))
        }
        ParameterKind::Enum => {
            let selected = value
                .as_str()
                .ok_or_else(|| type_error(plugin_id, p, "string enum"))?;
            if !p.options.iter().any(|x| x == selected) {
                return Err(BackendError::Plugin(format!(
                    "plugin '{}' parameter '{}' value '{}' is not one of {:?}",
                    plugin_id, p.id, selected, p.options
                )));
            }
            None
        }
        _ => None,
    };
    if let Some(n) = numeric {
        if !n.is_finite() {
            return Err(BackendError::Plugin(format!(
                "plugin '{}' parameter '{}' must be finite",
                plugin_id, p.id
            )));
        }
        if let Some(min) = p.min {
            if n < min {
                return Err(BackendError::Plugin(format!(
                    "plugin '{}' parameter '{}' must be >= {}",
                    plugin_id, p.id, min
                )));
            }
        }
        if let Some(max) = p.max {
            if n > max {
                return Err(BackendError::Plugin(format!(
                    "plugin '{}' parameter '{}' must be <= {}",
                    plugin_id, p.id, max
                )));
            }
        }
    }
    Ok(())
}

fn type_error(plugin_id: &str, p: &ParameterDescriptor, expected: &str) -> BackendError {
    BackendError::Plugin(format!(
        "plugin '{}' parameter '{}' must be {}",
        plugin_id, p.id, expected
    ))
}
