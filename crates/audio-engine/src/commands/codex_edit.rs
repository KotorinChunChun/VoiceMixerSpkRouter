//! AIの操作案を隔離したGraphで検証してから既存の復元処理で適用する。
use super::*;
use crate::audio::graph::{Edge, NodeLayout, PortId};
use crate::registry::node_registry::NodeRegistry;
use serde::Deserialize;
use std::{collections::HashMap, sync::atomic::Ordering};

fn invalid(message: impl Into<String>) -> BackendError {
    BackendError::InvalidOperation(message.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    summary: String,
    questions: Vec<String>,
    operations: Vec<Operation>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    op: String,
    node: String,
    key: String,
    value: String,
}

fn editable(node: &NodeSpec) -> Value {
    // 値だけでなくキーも限定する。パス、デバイスID、実行状態、外部アクションは送信しない。
    let raw = serde_json::to_value(&node.properties).unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    for key in [
        "gain_db",
        "mute",
        "bypass",
        "auto_bypass_on_error",
        "looped",
        "loop_wait_ms",
        "seconds",
        "include_process_tree",
        "capture_method",
        "failure_policy",
    ] {
        if let Some(value) = raw.get(key) {
            out.insert(format!("/{key}"), value.clone());
        }
    }
    fn numeric_leaves(value: &Value, path: &str, out: &mut serde_json::Map<String, Value>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key.ends_with("_id") || key.ends_with("_ids") {
                        continue;
                    }
                    numeric_leaves(
                        value,
                        &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                        out,
                    );
                }
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    numeric_leaves(value, &format!("{path}/{index}"), out);
                }
            }
            Value::Number(_) | Value::Bool(_) => {
                out.insert(path.into(), value.clone());
            }
            _ => {}
        }
    }
    if let Some(config) = raw.get("config") {
        numeric_leaves(config, "/config", &mut out);
    }
    if let Some(slots) = raw.get("slots").and_then(Value::as_array) {
        for (index, slot) in slots.iter().enumerate() {
            for key in ["gain_db", "mute"] {
                if let Some(value) = slot.get(key) {
                    out.insert(format!("/slots/{index}/{key}"), value.clone());
                }
            }
        }
    }
    if let Some(flow) = raw.get("flow").and_then(Value::as_array) {
        for (index, member) in flow.iter().enumerate() {
            for key in ["gain_db", "mute", "bypass", "parameters"] {
                if let Some(value) = member.get(key) {
                    numeric_leaves(value, &format!("/flow/{index}/{key}"), &mut out);
                }
            }
        }
    }
    if let Some(parameters) = raw.get("parameters").and_then(Value::as_object) {
        for (key, value) in parameters {
            if value.is_number() || value.is_boolean() {
                out.insert(
                    format!("/parameters/{}", key.replace('~', "~0").replace('/', "~1")),
                    value.clone(),
                );
            }
        }
    }
    Value::Object(out)
}

fn supported(node: &NodeSpec) -> bool {
    !matches!(
        node.node_type,
        NodeType::Action
            | NodeType::LogOutput
            | NodeType::Recorder
            | NodeType::ReplayCache
            | NodeType::MemoryReplay
    )
}

fn node_context(node: &NodeSpec) -> Value {
    json!({"id":node.id,"name":node.display_name,"type":node.node_type,
        "plugin_id":serde_json::to_value(&node.properties).ok().and_then(|p| p.get("plugin_id").cloned()),
        "inputs":node.input_ports,"outputs":node.output_ports,"editable":editable(node)})
}

fn context(backend: &Backend, expected: &Value) -> Result<Value> {
    let profile: RoutingProfile = serde_json::from_value(expected.clone())?;
    let registrations: Vec<_> = backend
        .registry
        .list()
        .into_iter()
        .filter(|r| r.enabled)
        .filter_map(|r| {
            let mut node = r.instantiate();
            node.input_ports = r.input_ports.clone();
            node.output_ports = r.output_ports.clone();
            if !supported(&node) {
                return None;
            }
            let mut value = node_context(&node);
            value["id"] = json!(r.id);
            value["instance_policy"] = json!(r.instance_policy);
            Some(value)
        })
        .collect();
    let mut plugins: Vec<_> = backend
        .plugins
        .list_plugins()
        .into_iter()
        .map(|p| {
            let parameters: Vec<_> = p
                .parameters
                .into_iter()
                .filter(|p| {
                    matches!(
                        p.kind,
                        crate::plugins::manifest::ParameterKind::Number
                            | crate::plugins::manifest::ParameterKind::Integer
                            | crate::plugins::manifest::ParameterKind::Boolean
                    )
                })
                .collect();
            json!({"id":p.plugin_id,"name":p.name,"parameters":parameters})
        })
        .collect();
    plugins.sort_by_key(|p| p["id"].as_str().unwrap_or_default().to_owned());
    let mut nodes: Vec<_> = profile
        .nodes
        .values()
        .filter(|n| supported(n))
        .map(node_context)
        .collect();
    nodes.sort_by_key(|n| n["id"].as_str().unwrap_or_default().to_owned());
    Ok(json!({"nodes":nodes,
        "edges":profile.routing_edges,"registrations":registrations,"plugins":plugins,
        "scope":"登録済み音声ノードの追加・削除・改名、音声接続、gain_db/mute/bypass、数値・真偽値のプラグインparameters。録音・外部アクション・イベント・ファイル・デバイス参照変更は対象外。"}))
}

fn resolve(ids: &HashMap<String, NodeId>, name: &str) -> Result<NodeId> {
    ids.get(name)
        .copied()
        .ok_or_else(|| invalid(format!("対象ノードがありません: {name}")))
}

fn candidate(backend: &Backend, expected: &Value, raw: Value) -> Result<Value> {
    let proposal: Proposal = serde_json::from_value(raw)?;
    if proposal.operations.len() > 100
        || proposal.summary.len() > 16000
        || proposal.questions.len() > 20
    {
        return Err(invalid("生成結果の件数・文字数が上限を超えています"));
    }
    if !proposal.questions.is_empty() {
        if !proposal.operations.is_empty() {
            return Err(invalid("質問と編集操作が同時に返されました"));
        }
        return Ok(
            json!({"status":"needs_input","summary":proposal.summary,"questions":proposal.questions}),
        );
    }
    if proposal.operations.is_empty() {
        return Err(invalid(
            "編集操作がありません。要求を具体的に入力してください",
        ));
    }
    let mut profile: RoutingProfile = serde_json::from_value(expected.clone())?;
    let staging = crate::controller::GraphController::new();
    staging.commit_graph_candidate(0, profile.graph(), |_| Ok(()))?;
    let registry = NodeRegistry::default();
    registry.replace_all(backend.registry.snapshot());
    let mut ids: HashMap<String, NodeId> = profile
        .nodes
        .values()
        .filter(|n| supported(n))
        .map(|n| (n.id.to_string(), n.id))
        .collect();
    let mut changes = Vec::<Value>::new();
    for op in proposal.operations {
        match op.op.as_str() {
            "add" => {
                if op.node.is_empty() || op.node.len() > 100 || ids.contains_key(&op.node) {
                    return Err(invalid("追加ノードのローカル名が不正・重複しています"));
                }
                let id = serde_json::from_value(json!(op.key))?;
                let mut node = registry.instantiate(id, &staging.snapshot())?;
                if !supported(&node) {
                    return Err(invalid("この種類のノードはAI編集対象外です"));
                }
                if !op.value.is_empty() {
                    node.display_name = op.value;
                }
                if node.display_name.len() > 500 {
                    return Err(invalid("ノード名が長すぎます"));
                }
                let node_id = node.id;
                changes.push(json!({"operation":"ノード追加","target":node.display_name,"before":null,"after":node_context(&node)}));
                staging.create_node(node)?;
                staging.update_node_layout(NodeLayout {
                    node_id,
                    x: 80.0 + ids.len() as f32 * 40.0,
                    y: 120.0,
                })?;
                ids.insert(op.node, node_id);
            }
            "remove" | "rename" | "set" => {
                let id = resolve(&ids, &op.node)?;
                let mut node = staging
                    .snapshot()
                    .nodes
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| invalid("削除済みノードです"))?;
                let name = node.display_name.clone();
                if op.op == "remove" {
                    if profile
                        .event_edges
                        .iter()
                        .any(|e| e.from_node == id || e.to_node == id)
                        || profile.shortcuts.values().any(|s| s.node_id == id)
                    {
                        return Err(invalid(
                            "イベント接続・ショートカットのあるノードはAIから削除できません",
                        ));
                    }
                    staging.delete_node(id)?;
                    changes.push(json!({"operation":"ノード削除（音声接続も削除）","target":name,"before":node_context(&node),"after":null}));
                } else if op.op == "rename" {
                    if op.value.trim().is_empty() || op.value.len() > 500 {
                        return Err(invalid("ノード名が不正です"));
                    }
                    changes.push(json!({"operation":"名前変更","target":name,"before":name,"after":op.value}));
                    node.display_name = op.value;
                    staging.update_node(node)?;
                } else {
                    let allowed = editable(&node);
                    let old = allowed
                        .get(&op.key)
                        .ok_or_else(|| invalid(format!("編集対象外の設定です: {}", op.key)))?;
                    let value: Value = serde_json::from_str(&op.value)?;
                    if !(old.is_number() && value.is_number()
                        || old.is_boolean() && value.is_boolean()
                        || old.is_string() && value.is_string())
                    {
                        return Err(invalid("設定値の型が異なります"));
                    }
                    if op.key.ends_with("/gain_db")
                        && !(-96.0..=24.0).contains(&value.as_f64().unwrap_or(f64::NAN))
                    {
                        return Err(invalid("音量は-96〜24dBで指定してください"));
                    }
                    let mut properties = serde_json::to_value(&node.properties)?;
                    *properties
                        .pointer_mut(&op.key)
                        .ok_or_else(|| invalid("設定が存在しません"))? = value.clone();
                    node.properties = serde_json::from_value(properties)?;
                    if let NodeProperties::TransformPlugin {
                        plugin_id,
                        parameters,
                        ..
                    } = &node.properties
                    {
                        backend.plugins.validate_parameters(plugin_id, parameters)?;
                    }
                    validate_audio_processing_group(backend, &node)?;
                    staging.update_node(node)?;
                    changes.push(json!({"operation":"設定変更","target":name,"key":op.key,"before":old,"after":value}));
                }
            }
            "connect" => {
                let from = resolve(&ids, &op.node)?;
                let to = resolve(&ids, &op.key)?;
                let indexes: [usize; 2] = serde_json::from_str(&op.value)?;
                let graph = staging.snapshot();
                let source = graph
                    .nodes
                    .get(&from)
                    .ok_or_else(|| invalid("接続元がありません"))?;
                let target = graph
                    .nodes
                    .get(&to)
                    .ok_or_else(|| invalid("接続先がありません"))?;
                let output: PortId = source
                    .output_ports
                    .get(indexes[0])
                    .ok_or_else(|| invalid("出力ポートがありません"))?
                    .id;
                let input: PortId = target
                    .input_ports
                    .get(indexes[1])
                    .ok_or_else(|| invalid("入力ポートがありません"))?
                    .id;
                staging.connect_nodes(from, output, to, input)?;
                changes.push(json!({"operation":"接続追加","target":format!("{} → {}",source.display_name,target.display_name),"before":null,"after":indexes}));
            }
            "disconnect" | "edge_set" => {
                let mut graph = staging.snapshot();
                let edge: &mut Edge = graph
                    .edges
                    .iter_mut()
                    .find(|e| e.id.to_string() == op.node)
                    .ok_or_else(|| invalid("接続がありません"))?;
                let description = format!(
                    "{} → {}",
                    graph.nodes[&edge.from_node].display_name,
                    graph.nodes[&edge.to_node].display_name
                );
                if op.op == "disconnect" {
                    staging.disconnect_nodes(edge.id)?;
                    changes.push(json!({"operation":"接続削除","target":description,"before":edge,"after":null}));
                } else {
                    let value: Value = serde_json::from_str(&op.value)?;
                    let old = match op.key.as_str() {
                        "gain_db" => {
                            let n = value
                                .as_f64()
                                .ok_or_else(|| invalid("接続音量が数値ではありません"))?;
                            if !(-96.0..=24.0).contains(&n) {
                                return Err(invalid("接続音量は-96〜24dBで指定してください"));
                            }
                            let old = json!(edge.gain_db);
                            edge.gain_db = n as f32;
                            old
                        }
                        "mute" => {
                            let old = json!(edge.mute);
                            edge.mute = value
                                .as_bool()
                                .ok_or_else(|| invalid("muteが真偽値ではありません"))?;
                            old
                        }
                        _ => return Err(invalid("接続の編集対象外の項目です")),
                    };
                    staging.commit_graph_candidate(graph.revision, graph, |_| Ok(()))?;
                    changes.push(json!({"operation":"接続設定変更","target":description,"key":op.key,"before":old,"after":value}));
                }
            }
            _ => return Err(invalid("未対応のAI操作です")),
        }
    }
    let graph = staging.snapshot();
    if !crate::routing::loop_detector::explicit_cycles(&graph).is_empty() {
        return Err(invalid("循環する音声接続は適用できません"));
    }
    if graph.nodes.len() > 500 || graph.edges.len() > 2000 {
        return Err(invalid("構成が上限を超えています"));
    }
    profile.nodes = graph.nodes;
    profile.routing_edges = graph.edges;
    profile.node_layouts = graph.layouts;
    // 未接続の必須入力は通常の編集中と同じ扱い。それ以外は保存プロファイル検証を通す。
    let mut validation = profile.clone();
    for node in validation.nodes.values_mut() {
        for port in &mut node.input_ports {
            port.required = false;
        }
    }
    validate_full_profile(&validation)?;
    Ok(
        json!({"status":"ready","summary":proposal.summary,"questions":[],"changes":changes,"target":profile}),
    )
}

pub fn dispatch(backend: &Backend, action: &str, payload: Value) -> Result<Value> {
    if action == "codex_context" {
        return context(backend, &snapshot(backend)?);
    }
    let mut session = backend.codex_config.job.lock();
    if action == "codex_start" {
        if session.as_ref().is_some_and(|j| j.result.lock().is_none()) {
            return Err(invalid("すでに生成中です"));
        }
        let request = payload["request"].as_str().unwrap_or("").trim();
        if request.is_empty() || request.len() > 16000 {
            return Err(invalid("要求は1〜16000バイトで入力してください"));
        }
        let expected = snapshot(backend)?;
        let input = context(backend, &expected)?;
        if serde_json::to_vec(&input)?.len() > 1024 * 1024 {
            return Err(invalid("構成情報が1MBを超えています"));
        }
        // 利用者が確認した送信情報と同じものだけを送る。
        if payload["context"] != input {
            return Err(invalid(
                "送信情報が変わりました。情報を再読み込みしてください",
            ));
        }
        let job = crate::codex_config::start(expected, input, request);
        let id = job.id.clone();
        *session = Some(job);
        return Ok(json!({"id":id,"status":"running"}));
    }
    let job = session
        .as_mut()
        .filter(|j| payload["id"].as_str() == Some(&j.id))
        .ok_or_else(|| invalid("生成セッションがありません"))?;
    if action == "codex_cancel" {
        job.cancelled.store(true, Ordering::Relaxed);
        return Ok(json!({"status":"cancelled"}));
    }
    if job.cancelled.load(Ordering::Relaxed) {
        return Err(invalid("この生成はキャンセルされました"));
    }
    if action == "codex_poll" {
        if job.preview.is_none() {
            let result = job.result.lock().clone();
            let Some(result) = result else {
                return Ok(json!({"status":"running"}));
            };
            job.preview = Some(match result {
                Ok(raw) => match candidate(backend, &job.expected, raw) {
                    Ok(value) => value,
                    Err(e) => json!({"status":"failed","error":e.to_string()}),
                },
                Err(error) => json!({"status":"failed","error":error}),
            });
        }
        let mut preview = job.preview.clone().unwrap();
        preview.as_object_mut().unwrap().remove("target");
        return Ok(preview);
    }
    if action == "codex_apply" {
        if snapshot(backend)? != job.expected {
            return Err(invalid(
                "生成後にプロジェクトが変更されました。変更案を作り直してください",
            ));
        }
        if context(backend, &job.expected)? != job.context {
            return Err(invalid(
                "利用可能なノードが変更されました。変更案を作り直してください",
            ));
        }
        let preview = job
            .preview
            .as_ref()
            .filter(|p| p["status"] == "ready")
            .ok_or_else(|| invalid("適用できる変更案がありません"))?;
        if backend.nodes.any_recorder_recording() {
            return Err(invalid("録音中はAI編集を適用できません"));
        }
        let id = job.expected["profile_id"].as_str().unwrap_or("unsaved");
        if routing_profile::is_user_project(id) {
            workflow_store::archive(
                &backend.config,
                id,
                &serde_json::to_vec(&job.expected)?,
                true,
            )?;
        }
        let result = restore(backend, preview["target"].clone())?;
        *session = None;
        return Ok(result);
    }
    Err(invalid("未対応のCodex操作です"))
}
