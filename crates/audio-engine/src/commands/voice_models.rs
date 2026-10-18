use crate::{audio::graph::NodeProperties, error::BackendError, Backend, Result};
use serde_json::Value;
use std::path::PathBuf;

pub fn execute(
    backend: &Backend,
    action: String,
    method: Option<String>,
    job_id: Option<String>,
) -> Result<Value> {
    let manager = {
        let mut stored = backend.voice_models.lock();
        if stored.is_none() {
            let config = backend.config.load()?;
            let package=config.settings.plugin_search_paths.iter().map(|path|backend.config.resolve_relative(path).join("voice-extraction")).find(|path|path.join("plugin.json").is_file()).ok_or_else(||BackendError::InvalidOperation("人声抽出パッケージがありません。配布plugins/voice-extractionを確認してください".into()))?;
            let root = if cfg!(debug_assertions) && package.join(".models").is_dir() {
                package.join(".models").canonicalize()?
            } else {
                backend.config.base_dir().join("models/voice-extraction")
            };
            *stored = Some(crate::plugins::voice_models::VoiceModelManager::new(
                package, root,
            )?);
        }
        stored.as_ref().unwrap().clone()
    };
    match action.as_str() {
        "list" => {
            let roots = backend
                .controller
                .snapshot()
                .nodes
                .values()
                .filter_map(|node| match &node.properties {
                    NodeProperties::TransformPlugin {
                        plugin_id,
                        parameters,
                        ..
                    } if plugin_id.contains("rvc") => parameters
                        .get("model")
                        .or_else(|| parameters.get("model_path"))
                        .and_then(|value| value.as_str())
                        .map(PathBuf::from)
                        .and_then(|path| path.parent().map(PathBuf::from)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            manager.list(&roots)
        }
        "install" => Ok(serde_json::to_value(
            manager.install(method.as_deref().unwrap_or(""))?,
        )?),
        "setup_runtime" => Ok(serde_json::to_value(manager.setup_runtime()?)?),
        "cancel" => Ok(serde_json::to_value(
            manager.cancel(job_id.as_deref().unwrap_or(""))?,
        )?),
        "query" => Ok(serde_json::to_value(
            manager.query(job_id.as_deref().unwrap_or(""))?,
        )?),
        "configuration" => Ok(manager.plugin_configuration()),
        "remove" => {
            let method = method.unwrap_or_default();
            let used =
                backend
                    .controller
                    .snapshot()
                    .nodes
                    .values()
                    .any(|node| match &node.properties {
                        NodeProperties::TransformPlugin {
                            plugin_id,
                            parameters,
                            ..
                        } => {
                            backend.nodes.is_active(node.id)
                                && plugin_id == crate::plugins::voice_models::PLUGIN_ID
                                && parameters
                                    .get("method")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("deepfilternet3")
                                    == method
                        }
                        _ => false,
                    });
            manager.remove(&method, used)
        }
        _ => Err(BackendError::InvalidOperation(
            "未対応のモデル操作です".into(),
        )),
    }
}
