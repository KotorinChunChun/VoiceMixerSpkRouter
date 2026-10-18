//! 操作パネルと変換処理グループだけのテンプレートを保存する。
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Library {
    #[serde(default)]
    panels: BTreeMap<String, Vec<Panel>>,
    #[serde(default)]
    templates: Vec<Template>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Control {
    node_id: NodeId,
    kind: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Panel {
    id: String,
    name: String,
    controls: Vec<Control>,
}
#[derive(Serialize, Deserialize)]
struct Template {
    id: String,
    name: String,
    clipboard: Value,
}

fn load(backend: &Backend) -> Result<Library> {
    let path = backend.config.base_dir().join("workflow-library.json");
    if !path.exists() {
        return Ok(Library::default());
    }
    if std::fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
        return Err(invalid("ライブラリが16MBを超えています"));
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
fn save(backend: &Backend, library: &Library) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(library)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(invalid("ライブラリが16MBを超えています"));
    }
    crate::config::transaction::atomic_write(
        &backend.config.base_dir().join("workflow-library.json"),
        &bytes,
    )
}
fn invalid(text: &str) -> BackendError {
    BackendError::InvalidOperation(text.into())
}
fn name(value: &Value) -> Result<String> {
    let text = value.as_str().unwrap_or("").trim();
    if text.is_empty() || text.chars().count() > 80 {
        return Err(invalid("名前は1〜80文字で入力してください"));
    }
    Ok(text.into())
}
fn project(backend: &Backend) -> Result<String> {
    backend
        .active_routing_profile
        .lock()
        .clone()
        .filter(|id| routing_profile::is_user_project(id))
        .ok_or_else(|| invalid("操作パネルは別名で保存したプロジェクトで利用できます"))
}

pub fn dispatch(backend: &Backend, action: &str, payload: Value) -> Result<Value> {
    let mut library = load(backend)?;
    match action {
        "library_get" => {
            let panels = backend
                .active_routing_profile
                .lock()
                .as_ref()
                .and_then(|id| library.panels.get(id))
                .cloned()
                .unwrap_or_default();
            Ok(
                json!({"panels":panels,"templates":library.templates.iter().map(|t|json!({"id":t.id,"name":t.name})).collect::<Vec<_>>()}),
            )
        }
        "panel_save" => {
            let project = project(backend)?;
            if payload
                .get("project_id")
                .is_some_and(|value| value.as_str() != Some(project.as_str()))
            {
                return Err(invalid(
                    "プロジェクトが切り替わっています。パネルを開き直してください",
                ));
            }
            let controls: Vec<Control> = serde_json::from_value(payload["controls"].clone())?;
            if controls.len() > 64 {
                return Err(invalid("操作項目は64件までです"));
            }
            let graph = backend.controller.snapshot();
            for control in &controls {
                if !["gain", "mute", "record", "selector"].contains(&control.kind.as_str()) {
                    return Err(invalid("未対応の操作項目です"));
                }
                if let Some(node) = graph.nodes.get(&control.node_id) {
                    if (control.kind == "record"
                        && !matches!(node.node_type, NodeType::Recorder | NodeType::ReplayCache))
                        || (control.kind == "selector" && node.node_type != NodeType::InputSelector)
                        || (["gain", "mute"].contains(&control.kind.as_str())
                            && matches!(node.node_type, NodeType::Action | NodeType::LogOutput))
                    {
                        return Err(invalid("このノードでは選択した操作を利用できません"));
                    }
                }
            }
            let panels = library.panels.entry(project).or_default();
            let id = match payload["id"].as_str().filter(|id| !id.is_empty()) {
                Some(id) => {
                    uuid::Uuid::parse_str(id).map_err(|_| invalid("パネルIDが不正です"))?;
                    id.to_owned()
                }
                None => uuid::Uuid::new_v4().to_string(),
            };
            let panel = Panel {
                id: id.clone(),
                name: name(&payload["name"])?,
                controls,
            };
            if let Some(old) = panels.iter_mut().find(|p| p.id == id) {
                *old = panel.clone();
            } else {
                if panels.len() >= 16 {
                    return Err(invalid("パネルはプロジェクトごとに16件までです"));
                }
                panels.push(panel.clone());
            }
            save(backend, &library)?;
            Ok(serde_json::to_value(panel)?)
        }
        "panel_delete" => {
            let project = project(backend)?;
            if payload
                .get("project_id")
                .is_some_and(|value| value.as_str() != Some(project.as_str()))
            {
                return Err(invalid(
                    "プロジェクトが切り替わっています。パネルを開き直してください",
                ));
            }
            let panels = library.panels.entry(project).or_default();
            panels.retain(|p| Some(p.id.as_str()) != payload["id"].as_str());
            save(backend, &library)?;
            Ok(Value::Null)
        }
        "template_save" => {
            let id: NodeId = serde_json::from_value(payload["node_id"].clone())?;
            let graph = backend.controller.snapshot();
            let node = graph
                .nodes
                .get(&id)
                .ok_or_else(|| invalid("選択したノードがありません"))?;
            if node.node_type != NodeType::AudioProcessingGroup {
                return Err(invalid(
                    "変換処理グループを1つ選択してください。入力・出力ノードは保存できません",
                ));
            }
            let name = name(&payload["name"])?;
            if library.templates.iter().any(|t| t.name == name) {
                return Err(invalid(
                    "同名のテンプレートがあります。別の名前を入力してください",
                ));
            }
            if library.templates.len() >= 100 {
                return Err(invalid("テンプレートは100件までです"));
            }
            let mut clipboard = export_graph_nodes(backend, vec![id])?;
            // グループは自己完結した処理設定。登録元への参照を持ち越さない。
            clipboard["nodes"][0]["registered_node_id"] = Value::Null;
            library.templates.push(Template {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                clipboard,
            });
            save(backend, &library)?;
            Ok(Value::Null)
        }
        "template_delete" => {
            library
                .templates
                .retain(|t| Some(t.id.as_str()) != payload["id"].as_str());
            save(backend, &library)?;
            Ok(Value::Null)
        }
        "template_apply" => {
            let template = library
                .templates
                .iter()
                .find(|t| Some(t.id.as_str()) == payload["id"].as_str())
                .ok_or_else(|| invalid("テンプレートがありません"))?;
            let clipboard: GraphClipboard = serde_json::from_value(template.clipboard.clone())?;
            if clipboard.nodes.len() != 1
                || clipboard.nodes[0].node_type != NodeType::AudioProcessingGroup
                || !clipboard.edges.is_empty()
                || !clipboard.event_edges.is_empty()
            {
                return Err(invalid(
                    "テンプレートに変換処理グループ以外が含まれています",
                ));
            }
            let position: GraphPosition = serde_json::from_value(payload["position"].clone())?;
            let specs = paste_graph_nodes(backend, template.clipboard.clone(), position)?;
            Ok(serde_json::to_value(specs)?)
        }
        _ => Err(invalid("未対応のライブラリ操作です")),
    }
}
