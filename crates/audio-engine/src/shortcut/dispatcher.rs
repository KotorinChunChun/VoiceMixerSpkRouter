use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam_channel::Receiver;

use crate::{
    audio::graph::{NodeId, NodeProperties},
    commands::tauri_commands as commands,
    error::{BackendError, Result},
    recording::state::RecorderState,
    Backend,
};

use super::model::{NodeActionDescriptor, ShortcutId};

pub struct NodeActionDispatcher;

impl NodeActionDispatcher {
    pub fn list_actions(backend: &Backend, node_id: NodeId) -> Result<Vec<NodeActionDescriptor>> {
        let node = backend
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        Ok(Self::list_actions_for_node(&node))
    }

    pub fn list_actions_for_node(
        node: &crate::audio::graph::NodeSpec,
    ) -> Vec<NodeActionDescriptor> {
        let mut actions = vec![descriptor("enable_toggle", "Enable / Disable", None)];
        if node.properties.gain_mute().is_some() {
            actions.push(descriptor("mute_toggle", "Mute Toggle", None));
            let volume_schema = Some(serde_json::json!({
                "type": "number",
                "minimum": 0.1,
                "maximum": 12.0,
                "default": 1.0,
                "description": "Gain step in dB"
            }));
            actions.push(descriptor("volume_up", "Volume Up", volume_schema.clone()));
            actions.push(descriptor("volume_down", "Volume Down", volume_schema));
        }
        match &node.properties {
            NodeProperties::Rvc { .. } | NodeProperties::TransformPlugin { .. } => {
                actions.push(descriptor("bypass", "Bypass Toggle", None));
            }
            NodeProperties::InputSelector { .. } => {
                for index in 0..node.input_ports.len() {
                    actions.push(descriptor(
                        &format!("input_{}", index + 1),
                        &format!("Input {}", index + 1),
                        None,
                    ));
                }
                actions.push(descriptor("next_input", "Next Input", None));
            }
            NodeProperties::Recorder { .. } | NodeProperties::ReplayCache { .. } => {
                actions.push(descriptor("recording_start", "Recording Start", None));
                actions.push(descriptor("recording_stop", "Recording Stop", None));
                actions.push(descriptor(
                    "recording_toggle",
                    "Recording Start / Stop",
                    None,
                ));
            }
            _ => {}
        }
        if matches!(node.properties, NodeProperties::TransformPlugin { .. }) {
            actions.push(descriptor(
                "apply_node_preset",
                "RVCプリセット切替",
                Some(serde_json::json!({
                    "type": "string",
                    "description": "preset name"
                })),
            ));
        }
        actions
    }

    pub fn validate(
        backend: &Backend,
        node_id: NodeId,
        action_id: &str,
        parameter: Option<&serde_json::Value>,
    ) -> Result<()> {
        let node = backend
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        Self::validate_node_action(&node, action_id, parameter)
    }

    pub fn validate_node_action(
        node: &crate::audio::graph::NodeSpec,
        action_id: &str,
        parameter: Option<&serde_json::Value>,
    ) -> Result<()> {
        let actions = Self::list_actions_for_node(node);
        if !actions.iter().any(|action| action.action_id == action_id) {
            return Err(BackendError::InvalidOperation(format!(
                "node does not expose shortcut action {action_id}"
            )));
        }
        if matches!(action_id, "volume_up" | "volume_down") {
            volume_step(parameter)?;
        } else if action_id == "apply_node_preset" {
            preset_name(parameter)?;
        } else if parameter.is_some() {
            return Err(BackendError::InvalidOperation(
                "shortcut action does not accept a parameter".into(),
            ));
        }
        Ok(())
    }

    pub fn execute(
        backend: &Backend,
        node_id: NodeId,
        action_id: &str,
        parameter: Option<&serde_json::Value>,
    ) -> Result<()> {
        Self::validate(backend, node_id, action_id, parameter)?;
        let node = backend
            .controller
            .snapshot()
            .nodes
            .get(&node_id)
            .cloned()
            .ok_or_else(|| BackendError::NodeNotFound(node_id.to_string()))?;
        match action_id {
            "mute_toggle" => {
                let (_, mute) = node.properties.gain_mute().ok_or_else(|| {
                    BackendError::InvalidOperation("node has no mute control".into())
                })?;
                commands::set_mute(backend, node_id, !mute)?;
            }
            "enable_toggle" => {
                let enabled = !node.runtime_enabled;
                commands::set_graph_runtime_enabled(backend, node_id, enabled)?;
                if enabled {
                    backend.start_runtime()?;
                    commands::start_node(backend, node_id)?;
                }
            }
            "volume_up" | "volume_down" => {
                let (gain_db, _) = node.properties.gain_mute().ok_or_else(|| {
                    BackendError::InvalidOperation("node has no gain control".into())
                })?;
                let direction = if action_id == "volume_up" { 1.0 } else { -1.0 };
                let gain_db = (gain_db + direction * volume_step(parameter)?).clamp(-120.0, 24.0);
                commands::set_gain(backend, node_id, gain_db)?;
            }
            "bypass" => match node.properties {
                NodeProperties::TransformPlugin { bypass, .. } => {
                    commands::set_transform_plugin_bypass(backend, node_id, !bypass)?;
                }
                NodeProperties::Rvc {
                    instance_id,
                    bypass,
                    ..
                } => {
                    commands::bypass_rvc(backend, instance_id, !bypass)?;
                    commands::save_current_configuration(backend)?;
                }
                _ => unreachable!(),
            },
            "next_input" => {
                commands::select_next_input(backend, node_id)?;
            }
            action if action.starts_with("input_") => {
                let index = action
                    .strip_prefix("input_")
                    .and_then(|value| value.parse::<usize>().ok())
                    .and_then(|value| value.checked_sub(1))
                    .ok_or_else(|| {
                        BackendError::InvalidOperation("invalid selector shortcut action".into())
                    })?;
                commands::select_input_by_index(backend, node_id, index)?;
            }
            "recording_start" => {
                commands::start_recorder(backend, node_id)?;
            }
            "recording_stop" => {
                commands::stop_recorder(backend, node_id)?;
            }
            "recording_toggle" => {
                let snapshot = commands::get_recorder_state(backend, node_id)?;
                if snapshot.session_active
                    || matches!(
                        snapshot.state,
                        RecorderState::Recording
                            | RecorderState::WaitingForInput
                            | RecorderState::Stopping
                    )
                {
                    commands::stop_recorder(backend, node_id)?;
                } else {
                    commands::start_recorder(backend, node_id)?;
                }
            }
            "apply_node_preset" => {
                commands::apply_node_preset(backend, node_id, preset_name(parameter)?)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

fn descriptor(
    action_id: &str,
    display_name: &str,
    parameter_schema: Option<serde_json::Value>,
) -> NodeActionDescriptor {
    NodeActionDescriptor {
        action_id: action_id.into(),
        display_name: display_name.into(),
        parameter_schema,
    }
}

fn volume_step(parameter: Option<&serde_json::Value>) -> Result<f32> {
    let step = parameter.and_then(serde_json::Value::as_f64).unwrap_or(1.0);
    if !step.is_finite() || !(0.1..=12.0).contains(&step) {
        return Err(BackendError::InvalidOperation(
            "volume shortcut step must be between 0.1 and 12 dB".into(),
        ));
    }
    Ok(step as f32)
}

fn preset_name(parameter: Option<&serde_json::Value>) -> Result<String> {
    let name = parameter
        .and_then(|value| {
            value.as_str().map(str::to_string).or_else(|| {
                value
                    .get("preset_name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
        })
        .unwrap_or_default();
    if name.trim().is_empty() {
        return Err(BackendError::InvalidOperation(
            "apply_node_preset requires preset_name".into(),
        ));
    }
    Ok(name)
}

pub fn spawn_dispatcher(
    backend: Weak<Backend>,
    triggers: Receiver<ShortcutId>,
) -> Result<ShortcutDispatchHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let join = thread::Builder::new()
        .name("shortcut-action-dispatcher".into())
        .spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                let Ok(shortcut_id) = triggers.recv_timeout(Duration::from_millis(20)) else {
                    continue;
                };
                let Some(backend) = backend.upgrade() else {
                    break;
                };
                let Some(binding) = backend.shortcut_engine.get(shortcut_id) else {
                    continue;
                };
                if !binding.enabled {
                    continue;
                }
                let outcome = NodeActionDispatcher::execute(
                    &backend,
                    binding.node_id,
                    &binding.action_id,
                    binding.action_parameter.as_ref(),
                )
                .map_err(|error| error.to_string());
                backend.shortcut_engine.report_triggered(&binding, outcome);
            }
        })?;
    Ok(ShortcutDispatchHandle {
        stop,
        join: Some(join),
    })
}

pub struct ShortcutDispatchHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for ShortcutDispatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
