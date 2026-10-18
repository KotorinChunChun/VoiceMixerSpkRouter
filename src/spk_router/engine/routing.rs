use crate::config::{Mode, Settings};
use audio_mixer_backend::{
    audio::graph::{
        ApplicationFilter, Edge, EndpointCaptureMode, GraphSnapshot, NodeId, NodeProperties,
        NodeSpec,
    },
    config::model::PersistedConfiguration,
    windows::{
        devices::{resolve_endpoint_id, DeviceDirection},
        process_watcher::normalize_exe_path,
    },
};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Route {
    pub(super) input: Option<NodeProperties>,
    pub(super) outputs: [Option<String>; 2],
}

fn resolve_route_with(
    settings: &Settings,
    own_executable: Option<&Path>,
    mut resolve: impl FnMut(&str) -> Result<String, String>,
) -> Result<Route, String> {
    settings.validate()?;
    if settings.mode == Mode::Stopped || !settings.mode_enabled(settings.mode) {
        return Ok(Route {
            input: None,
            outputs: [None, None],
        });
    }
    let input = match settings.mode {
        Mode::Device => NodeProperties::EndpointLoopback {
            endpoint_id: resolve(&settings.device_id)?,
            filter: ApplicationFilter::default(),
            capture_mode: EndpointCaptureMode::EndpointLoopback,
            gain_db: 0.0,
            mute: false,
        },
        Mode::Application => {
            if settings.application_path.as_os_str().is_empty() {
                return Err("取得するアプリを選択してください".into());
            }
            if own_executable.is_some_and(|own| {
                normalize_exe_path(own) == normalize_exe_path(&settings.application_path)
            }) {
                return Err("VoiceMixerSpkRouter自身の音声は取得できません".into());
            }
            NodeProperties::ProcessExe {
                exe_path: settings.application_path.clone(),
                gain_db: 0.0,
                mute: false,
            }
        }
        Mode::Browser => NodeProperties::EdgeTab {
            stream_key: settings.browser_stream_key.clone(),
            gain_db: 0.0,
            mute: false,
        },
        Mode::Stopped => unreachable!(),
    };
    let mut outputs: [Option<String>; 2] = [None, None];
    for (index, output) in settings.outputs.iter().enumerate() {
        if !output.enabled {
            continue;
        }
        let endpoint = resolve(&output.endpoint_id)?;
        if matches!(&input, NodeProperties::EndpointLoopback { endpoint_id, .. } if endpoint_id == &endpoint)
        {
            return Err(format!(
                "取得元と出力{}（{}）が同じ再生デバイスです。別のデバイスを選択してください",
                index + 1,
                settings.output_labels[index]
            ));
        }
        if outputs.iter().flatten().any(|other| other == &endpoint) {
            // 保存されたON/OFFは変えず、1→2の順で同一実デバイスへの二重送信を抑止。
            continue;
        }
        outputs[index] = Some(endpoint);
    }
    Ok(Route {
        input: Some(input),
        outputs,
    })
}

pub(super) fn resolve_route(settings: &Settings) -> Result<Route, String> {
    resolve_route_with(settings, std::env::current_exe().ok().as_deref(), |id| {
        resolve_endpoint_id(id, DeviceDirection::Render).map_err(|error| error.to_string())
    })
}

pub(super) fn project(settings: &Settings) -> (PersistedConfiguration, NodeId, [NodeId; 2]) {
    let properties = NodeProperties::EndpointLoopback {
        endpoint_id: settings.device_id.clone(),
        filter: ApplicationFilter::default(),
        capture_mode: EndpointCaptureMode::EndpointLoopback,
        gain_db: 0.0,
        mute: true,
    };
    let mut input = NodeSpec::new(properties.node_type(), "取得元", properties);
    input.runtime_enabled = false;
    let outputs: [NodeSpec; 2] = std::array::from_fn(|index| {
        let properties = NodeProperties::Monitor {
            endpoint_id: settings.outputs[index].endpoint_id.clone(),
            gain_db: 0.0,
            mute: true,
        };
        let mut node = NodeSpec::new(
            properties.node_type(),
            format!("出力{}", index + 1),
            properties,
        );
        node.runtime_enabled = false;
        node
    });
    let mut config = PersistedConfiguration::default();
    config.settings.plugin_search_paths.clear();
    config.settings.render_exclusive = false;
    config.settings.edge_port = settings.browser_port;
    config.settings.edge_auth_token = settings.browser_token.clone();
    // 論理既定はこの制御スレッドで解決し、検証した実EndpointだけをGraphへ渡す。
    // Backend側の既定切替が循環検査より先に走ることを防ぐ。
    for output in &outputs {
        config.routing_edges.push(Edge::new(
            input.id,
            input.output_ports[0].id,
            output.id,
            output.input_ports[0].id,
        ));
    }
    let input_id = input.id;
    let output_ids = [outputs[0].id, outputs[1].id];
    for node in [input, outputs[0].clone(), outputs[1].clone()] {
        config.nodes.insert(node.id, node);
    }
    (config, input_id, output_ids)
}

pub(super) fn configure_graph(
    graph: &mut GraphSnapshot,
    input: NodeId,
    outputs: [NodeId; 2],
    route: &Route,
) {
    let node = graph.nodes.get_mut(&input).unwrap();
    if let Some(properties) = &route.input {
        node.properties = properties.clone();
        node.node_type = properties.node_type();
    }
    node.runtime_enabled = route.input.is_some();
    node.properties.set_mute(route.input.is_none());
    for (id, endpoint) in outputs.into_iter().zip(&route.outputs) {
        let node = graph.nodes.get_mut(&id).unwrap();
        if let Some(endpoint) = endpoint {
            node.properties = NodeProperties::Monitor {
                endpoint_id: endpoint.clone(),
                gain_db: 0.0,
                mute: false,
            };
        }
        node.runtime_enabled = endpoint.is_some();
        node.properties.set_mute(endpoint.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn settings(mode: Mode) -> Settings {
        let mut value = Settings::default();
        value.mode = mode;
        value.device_id = "source".into();
        value.application_path = PathBuf::from("C:/Applications/player.exe");
        value.outputs[0].endpoint_id = "speaker".into();
        value.outputs[0].enabled = true;
        value
    }

    #[test]
    fn loopback_rejects_aliases_resolving_to_the_same_device() {
        let value = settings(Mode::Device);
        let error = resolve_route_with(&value, None, |_| Ok("same".into())).unwrap_err();
        assert!(error.contains("取得元と出力1"));
    }

    #[test]
    fn duplicate_outputs_are_suppressed_after_alias_resolution_without_changing_settings() {
        let mut value = settings(Mode::Browser);
        value.outputs[1].endpoint_id = "communications-default".into();
        value.outputs[1].enabled = true;
        let route = resolve_route_with(&value, None, |_| Ok("speaker".into())).unwrap();
        assert_eq!(route.outputs, [Some("speaker".into()), None]);
        assert!(value.outputs.iter().all(|output| output.enabled));
    }

    #[test]
    fn disabled_output_does_not_resolve_or_block_the_enabled_route() {
        let value = settings(Mode::Browser);
        let route = resolve_route_with(&value, None, |id| {
            assert_eq!(id, "speaker");
            Ok(id.into())
        })
        .unwrap();
        assert_eq!(route.outputs, [Some("speaker".into()), None]);
    }

    #[test]
    fn exactly_one_input_is_available_and_stop_disables_all_workers() {
        let value = settings(Mode::Stopped);
        let (project, input, outputs) = project(&value);
        let mut graph = GraphSnapshot {
            nodes: project.nodes,
            edges: project.routing_edges,
            ..Default::default()
        };
        for mode in Mode::ALL {
            let value = settings(mode);
            let route = resolve_route_with(&value, None, |id| Ok(id.into())).unwrap();
            configure_graph(&mut graph, input, outputs, &route);
            assert_eq!(
                graph
                    .nodes
                    .values()
                    .filter(|node| node.node_type.class()
                        == audio_mixer_backend::audio::graph::NodeClass::Input)
                    .count(),
                1
            );
            assert_eq!(graph.nodes[&input].runtime_enabled, mode != Mode::Stopped);
            assert_eq!(
                graph.nodes[&outputs[0]].runtime_enabled,
                mode != Mode::Stopped
            );
            assert!(!graph.nodes[&outputs[1]].runtime_enabled);
        }
    }

    #[test]
    fn stopped_mode_does_not_resolve_any_hardware_and_all_outputs_off_still_captures() {
        let mut value = settings(Mode::Stopped);
        resolve_route_with(&value, None, |_| panic!("停止中はEndpointを解決しない")).unwrap();
        value.mode = Mode::Browser;
        value.outputs[0].enabled = false;
        let route = resolve_route_with(&value, None, |_| panic!("OFFの出力を解決しない")).unwrap();
        assert!(route.input.is_some());
        assert_eq!(route.outputs, [None, None]);
    }

    #[test]
    fn own_application_and_unselected_application_are_rejected() {
        let mut value = settings(Mode::Application);
        assert!(
            resolve_route_with(&value, Some(&value.application_path), |id| Ok(id.into()))
                .unwrap_err()
                .contains("自身")
        );
        value.application_path.clear();
        assert!(resolve_route_with(&value, None, |id| Ok(id.into()))
            .unwrap_err()
            .contains("選択"));
    }
}
