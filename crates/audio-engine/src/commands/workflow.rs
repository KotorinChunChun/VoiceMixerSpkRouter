//! v0.4.8の編集状態と復元履歴。Tauri境界はtauri_commandsに集約する。
use super::*;
use crate::config::workflow_store;
use serde_json::{json, Value};
#[path = "codex_edit.rs"]
mod codex_edit;
#[path = "workflow_library.rs"]
mod library;

fn snapshot(backend: &Backend) -> Result<Value> {
    let id = backend
        .active_routing_profile
        .lock()
        .clone()
        .unwrap_or_else(|| "unsaved".into());
    let (name, description) = if routing_profile::is_user_project(&id) {
        let stored = routing_profile::load(&backend.config, &id)?;
        (stored.display_name, stored.description)
    } else {
        (String::new(), String::new())
    };
    let mut value = serde_json::to_value(profile_from_backend(backend, id, name, description))?;
    // 実行状態は編集履歴ではない。保存・復帰時の通常の開始処理に任せる。
    if let Some(nodes) = value["nodes"].as_object_mut() {
        for node in nodes.values_mut() {
            node["state"] = json!("stopped");
        }
    }
    Ok(value)
}

fn apply_edit_state(backend: &Backend, state: FullProfileState) -> Result<()> {
    let revision = backend.controller.snapshot().revision;
    backend
        .controller
        .commit_graph_candidate(revision, state.graph, |_| Ok(()))?;
    backend.registry.replace_all(state.registered_nodes);
    backend
        .action_engine
        .replace_obs_connections(state.obs_connections)?;
    backend.shortcut_engine.replace_bindings(state.shortcuts)?;
    backend.replay_caches.replace(state.replay_cache_metadata);
    *backend.active_routing_profile.lock() = state.active_profile;
    Ok(())
}

fn restore(backend: &Backend, value: Value) -> Result<Value> {
    if backend.nodes.any_recorder_recording() {
        return Err(BackendError::InvalidOperation(
            "録音中は構成を復元できません。録音を停止してください".into(),
        ));
    }
    let profile: RoutingProfile = serde_json::from_value(value)?;
    let current = snapshot(backend)?;
    if current["profile_id"] != profile.profile_id {
        return Err(BackendError::InvalidOperation(
            "別プロジェクトの編集履歴は適用できません".into(),
        ));
    }
    // 編集中は未接続の必須プラグイン入力を許す（通常の配置操作と同じ）。
    let mut validation_profile = profile.clone();
    for node in validation_profile.nodes.values_mut() {
        for port in &mut node.input_ports {
            port.required = false;
        }
    }
    validate_full_profile(&validation_profile)?;
    let previous = capture_full_profile_state(backend);
    let running = backend.runtime.is_running();
    let next = FullProfileState {
        graph: profile.graph(),
        registered_nodes: profile.registered_nodes,
        obs_connections: profile.obs_connections,
        shortcuts: profile.shortcuts,
        replay_cache_metadata: profile.replay_cache_metadata,
        active_profile: previous.active_profile.clone(),
    };
    // 配置だけのUndoで配信を中断しない。
    let mut comparison = next.graph.clone();
    comparison.layouts = previous.graph.layouts.clone();
    comparison.revision = previous.graph.revision;
    let mut old = previous.graph.clone();
    for node in old.nodes.values_mut() {
        node.state = RuntimeState::Stopped;
    }
    if serde_json::to_value(&comparison)? == serde_json::to_value(&old)?
        && serde_json::to_value(&next.registered_nodes)?
            == serde_json::to_value(&previous.registered_nodes)?
        && serde_json::to_value(&next.shortcuts)? == serde_json::to_value(&previous.shortcuts)?
        && serde_json::to_value(&next.obs_connections)?
            == serde_json::to_value(&previous.obs_connections)?
        && serde_json::to_value(&next.replay_cache_metadata)?
            == serde_json::to_value(&previous.replay_cache_metadata)?
    {
        backend.controller.commit_graph_candidate(
            previous.graph.revision,
            GraphSnapshot {
                layouts: next.graph.layouts,
                ..previous.graph.clone()
            },
            |graph| save_graph_configuration(backend, graph.clone()),
        )?;
        return snapshot(backend);
    }
    backend.input_monitor.stop_all();
    stop_audition(backend);
    // Graphの実行threadは維持し、未接続ノードを含む編集状態にも戻れるようにする。
    backend.nodes.stop_graph();
    let result = apply_edit_state(backend, next).and_then(|_| save_current_configuration(backend));
    if let Err(error) = result {
        apply_edit_state(backend, previous)?;
        if running {
            backend.nodes.start_graph();
        }
        return Err(error);
    }
    if running {
        backend.nodes.start_graph();
    }
    snapshot(backend)
}

pub fn dispatch(backend: &Backend, action: String, payload: Value) -> Result<Value> {
    let _operation = backend.operation_lock.lock();
    if serde_json::to_vec(&payload)?.len() > 16 * 1024 * 1024 {
        return Err(BackendError::InvalidOperation(
            "要求が16MBを超えています".into(),
        ));
    }
    match action.as_str() {
        "speech_settings" => Ok(serde_json::to_value(crate::speech_cli::load_settings(
            &backend.config,
        )?)?),
        "speech_save_settings" => {
            crate::speech_cli::save_settings(&backend.config, serde_json::from_value(payload)?)?;
            Ok(json!({"ok": true}))
        }
        "speech_poll" => Ok(serde_json::to_value(backend.speech_cli.poll())?),
        "speech_cancel" => {
            backend
                .speech_cli
                .cancel(payload["id"].as_str().unwrap_or_default())?;
            Ok(json!({"ok": true}))
        }
        "speech_start" => {
            let kind = payload["kind"].as_str().unwrap_or_default();
            let text = payload["text"].as_str().unwrap_or_default().to_owned();
            let audio = payload["audio"].as_str().map(PathBuf::from);
            Ok(serde_json::to_value(backend.speech_cli.start(
                &backend.config,
                kind,
                text,
                audio,
            )?)?)
        }
        "speech_output_directory" => Ok(json!(backend
            .config
            .base_dir()
            .join("speech-output")
            .join("recordings"))),
        "codex_context" | "codex_start" | "codex_poll" | "codex_cancel" | "codex_apply" => {
            codex_edit::dispatch(backend, &action, payload)
        }
        "library_get" | "panel_save" | "panel_delete" | "template_save" | "template_apply"
        | "template_delete" => library::dispatch(backend, &action, payload),
        "snapshot" => snapshot(backend),
        "restore_edit" => {
            let current = snapshot(backend)?;
            let expected = payload
                .get("expected")
                .ok_or_else(|| BackendError::InvalidOperation("編集前の状態がありません".into()))?;
            let target = payload
                .get("target")
                .ok_or_else(|| BackendError::InvalidOperation("復元先がありません".into()))?;
            if current["profile_id"] != expected["profile_id"]
                || current["profile_id"] != target["profile_id"]
            {
                return Err(BackendError::InvalidOperation(
                    "プロジェクトが切り替わっています".into(),
                ));
            }
            restore(
                backend,
                workflow_store::apply_delta(&current, expected, target)?,
            )
        }
        "history" | "checkpoint" | "restore_revision" => {
            let id = backend
                .active_routing_profile
                .lock()
                .clone()
                .filter(|id| routing_profile::is_user_project(id))
                .ok_or_else(|| {
                    BackendError::InvalidOperation(
                        "別名で保存したプロジェクトで利用できます".into(),
                    )
                })?;
            let history = workflow_store::revisions(&backend.config, &id)?;
            if action == "checkpoint" || action == "restore_revision" {
                workflow_store::archive(
                    &backend.config,
                    &id,
                    &serde_json::to_vec(&snapshot(backend)?)?,
                    true,
                )?;
            }
            if action == "restore_revision" {
                let wanted = payload["id"].as_str().unwrap_or("");
                let revision = history
                    .into_iter()
                    .find(|r| r.id == wanted)
                    .ok_or_else(|| {
                        BackendError::InvalidOperation("復元履歴が見つかりません".into())
                    })?;
                return restore(backend, revision.profile);
            }
            let history = workflow_store::revisions(&backend.config, &id)?;
            Ok(json!(history.into_iter().map(|r| json!({"id":r.id,"saved_at":r.saved_at,"label":r.label,"nodes":r.profile["nodes"].as_object().map_or(0, |n| n.len())})).collect::<Vec<_>>()))
        }
        _ => Err(BackendError::InvalidOperation(
            "未対応のワークフロー操作です".into(),
        )),
    }
}
