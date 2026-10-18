use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::audio::graph::PortDirection;

pub const TRANSFORM_PLUGIN_API_VERSION: u32 = 1;
pub const TRANSFORM_PLUGIN_API_VERSION_V2: u32 = 2;
pub const MAX_PLUGIN_PORTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginExecutionMode {
    BuiltIn,
    Native,
    Process,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginPortDescriptor {
    pub id: String,
    pub name: String,
    pub direction: PortDirection,
    pub required: bool,
    /// None means unlimited. Most transforms use Some(1).
    pub max_connections: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    Boolean,
    Integer,
    Number,
    String,
    File,
    Enum,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParameterDescriptor {
    pub id: String,
    pub label: String,
    pub kind: ParameterKind,
    pub default: Value,
    pub min: Option<f64>,
    pub max: Option<f64>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Optional UI section name. Descriptor order remains the display order.
    #[serde(default)]
    pub group: String,
    /// Human-readable unit such as `dB`, `秒`, or `半音`.
    #[serde(default)]
    pub unit: String,
    /// Recommended increment for number inputs and sliders.
    #[serde(default)]
    pub step: Option<f64>,
    /// True when applying the value requires recreating the plugin process.
    #[serde(default)]
    pub restart_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginAudioRequirements {
    pub preferred_sample_rate: Option<u32>,
    pub preferred_channels: Option<u16>,
    pub preferred_block_frames: Option<usize>,
    pub minimum_block_frames: Option<usize>,
    pub maximum_block_frames: Option<usize>,
}

impl Default for PluginAudioRequirements {
    fn default() -> Self {
        Self {
            preferred_sample_rate: Some(48_000),
            preferred_channels: Some(2),
            preferred_block_frames: None,
            minimum_block_frames: None,
            maximum_block_frames: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginDescriptor {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub category: String,
    pub execution_mode: PluginExecutionMode,
    pub input_ports: Vec<PluginPortDescriptor>,
    pub output_ports: Vec<PluginPortDescriptor>,
    pub parameters: Vec<ParameterDescriptor>,
    pub audio: PluginAudioRequirements,
    /// Algorithmic latency declared by the plugin, in audio frames.
    #[serde(default)]
    pub reported_latency_frames: usize,
    /// Frontend may show the plugin but should not automatically insert/enable it.
    pub default_enabled: bool,
}

impl PluginDescriptor {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !matches!(
            self.api_version,
            TRANSFORM_PLUGIN_API_VERSION | TRANSFORM_PLUGIN_API_VERSION_V2
        ) {
            return Err(format!(
                "unsupported transform plugin API version {}",
                self.api_version
            ));
        }
        if self.api_version == TRANSFORM_PLUGIN_API_VERSION
            && (self.input_ports.len() != 1 || self.output_ports.len() != 1)
        {
            return Err(
                "Transform Plugin API v1 requires exactly one input and one output port".into(),
            );
        }
        if self.input_ports.is_empty() || self.output_ports.is_empty() {
            return Err(
                "transform plugin must declare at least one input and one output port".into(),
            );
        }
        if self.input_ports.len() > MAX_PLUGIN_PORTS || self.output_ports.len() > MAX_PLUGIN_PORTS {
            return Err(format!(
                "plugin ports exceed maximum of {MAX_PLUGIN_PORTS} per direction"
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for port in self.input_ports.iter().chain(self.output_ports.iter()) {
            if port.id.trim().is_empty() || !ids.insert(port.id.clone()) {
                return Err("plugin port ids must be non-empty and unique".into());
            }
            if port.max_connections == Some(0) {
                return Err(format!(
                    "port '{}' max_connections must be positive or null",
                    port.id
                ));
            }
        }
        if self.api_version == TRANSFORM_PLUGIN_API_VERSION
            && (self.input_ports[0].direction != PortDirection::Input
                || self.output_ports[0].direction != PortDirection::Output)
        {
            return Err("plugin API v1 port directions are invalid".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginDescriptor, TRANSFORM_PLUGIN_API_VERSION_V2};

    fn manifest_json(api_version: u32, inputs: usize, outputs: usize) -> String {
        let make_port = |index: usize, direction: &str, required: bool| {
            format!(
                r#"{{"id":"{direction}_{index}","name":"{direction} {index}","direction":"{direction}","required":{required},"max_connections":1}}"#
            )
        };
        let input_ports = (0..inputs)
            .map(|index| make_port(index, "input", index == 0))
            .collect::<Vec<_>>()
            .join(",");
        let output_ports = (0..outputs)
            .map(|index| make_port(index, "output", true))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"plugin_id":"test.v2","name":"Test v2","version":"1.0.0","api_version":{api_version},"category":"transform","execution_mode":"process","input_ports":[{input_ports}],"output_ports":[{output_ports}],"parameters":[],"audio":{{}},"reported_latency_frames":960,"default_enabled":true}}"#
        )
    }

    #[test]
    fn t07_u_01_v2_manifest_accepts_multiple_ports_sidechain_and_latency() {
        let descriptor: PluginDescriptor =
            serde_json::from_str(&manifest_json(TRANSFORM_PLUGIN_API_VERSION_V2, 2, 2)).unwrap();
        assert_eq!(descriptor.input_ports.len(), 2);
        assert!(!descriptor.input_ports[1].required);
        assert_eq!(descriptor.reported_latency_frames, 960);
        descriptor.validate().unwrap();
    }

    #[test]
    fn t07_u_02_v1_manifest_defaults_latency_to_zero() {
        let mut json = manifest_json(1, 1, 1);
        json = json.replace(",\"reported_latency_frames\":960", "");
        let descriptor: PluginDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(descriptor.reported_latency_frames, 0);
        descriptor.validate().unwrap();
    }

    #[test]
    fn t07_u_03_v2_manifest_rejects_more_than_eight_ports() {
        let descriptor: PluginDescriptor =
            serde_json::from_str(&manifest_json(TRANSFORM_PLUGIN_API_VERSION_V2, 9, 1)).unwrap();
        let error = descriptor.validate().unwrap_err();
        assert!(error.contains("maximum of 8"));
    }
}

impl PluginDescriptor {
    pub fn default_parameters(&self) -> BTreeMap<String, Value> {
        self.parameters
            .iter()
            .map(|p| (p.id.clone(), p.default.clone()))
            .collect()
    }
}
