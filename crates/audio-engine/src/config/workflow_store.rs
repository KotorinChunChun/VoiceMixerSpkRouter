//! プロジェクトの世代保存。パスはアプリ管理下で採番する。
use crate::{
    config::{persistence::ConfigStore, transaction::atomic_write},
    error::{BackendError, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

const LIMIT: usize = 50;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Revision {
    pub id: String,
    pub project_id: String,
    pub saved_at: i64,
    pub label: String,
    pub profile: Value,
}

fn directory(config: &ConfigStore, project: &str) -> PathBuf {
    config
        .base_dir()
        .join("project-history")
        .join(format!("{:x}", Sha256::digest(project.as_bytes())))
}

pub fn revisions(config: &ConfigStore, project: &str) -> Result<Vec<Revision>> {
    let path = directory(config, project);
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut result = vec![];
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if entry.metadata()?.len() > MAX_BYTES as u64 {
            continue;
        }
        let value: Revision = serde_json::from_slice(&fs::read(entry.path())?)?;
        if value.project_id == project {
            result.push(value);
        }
    }
    result.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then(b.id.cmp(&a.id)));
    Ok(result)
}

pub fn archive(config: &ConfigStore, project: &str, bytes: &[u8], force: bool) -> Result<()> {
    if bytes.len() > MAX_BYTES {
        return Err(BackendError::InvalidOperation(
            "復元履歴の上限16MBを超えています".into(),
        ));
    }
    let root = directory(config, project);
    // 音量調整の自動保存ごとに全世代のJSONを読み込まない。
    if !force && root.exists() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.path().extension().and_then(|x| x.to_str()) == Some("json")
                && entry
                    .metadata()?
                    .modified()?
                    .elapsed()
                    .is_ok_and(|age| age.as_secs() < 60)
            {
                return Ok(());
            }
        }
    }
    let profile: Value = serde_json::from_slice(bytes)?;
    let previous = revisions(config, project)?;
    let now = chrono::Utc::now().timestamp_millis();
    if previous
        .first()
        .is_some_and(|last| last.profile == profile || (!force && now - last.saved_at < 60_000))
    {
        return Ok(());
    }
    let revision = Revision {
        id: uuid::Uuid::new_v4().to_string(),
        project_id: project.into(),
        saved_at: previous
            .first()
            .map_or(now, |last| now.max(last.saved_at.saturating_add(1))),
        label: if force {
            "手動保存・復元前"
        } else {
            "編集前の自動保存"
        }
        .into(),
        profile,
    };
    atomic_write(
        &root.join(format!("{}.json", revision.id)),
        &serde_json::to_vec(&revision)?,
    )?;
    for old in previous.iter().skip(LIMIT - 1) {
        let id = uuid::Uuid::parse_str(&old.id)
            .map_err(|_| BackendError::InvalidOperation("復元履歴IDが不正です".into()))?;
        fs::remove_file(root.join(format!("{id}.json")))?;
    }
    Ok(())
}

/// 変更したフィールドだけを復帰する。無関係な後続変更は保持し、競合は拒否する。
pub fn apply_delta(current: &Value, expected: &Value, target: &Value) -> Result<Value> {
    if expected == target {
        return Ok(current.clone());
    }
    if let (Some(c), Some(e), Some(t)) = (
        current.as_object(),
        expected.as_object(),
        target.as_object(),
    ) {
        let mut result = c.clone();
        let keys: std::collections::BTreeSet<_> = e.keys().chain(t.keys()).collect();
        for key in keys {
            match (e.get(key), t.get(key)) {
                (Some(before), Some(after)) => {
                    let next = apply_delta(c.get(key).unwrap_or(&Value::Null), before, after)?;
                    result.insert(key.clone(), next);
                }
                (None, Some(after)) => {
                    if c.contains_key(key) {
                        return Err(conflict());
                    }
                    result.insert(key.clone(), after.clone());
                }
                (Some(before), None) => {
                    if c.get(key) != Some(before) {
                        return Err(conflict());
                    }
                    result.remove(key);
                }
                _ => {}
            }
        }
        return Ok(Value::Object(result));
    }
    if current != expected {
        return Err(conflict());
    }
    Ok(target.clone())
}
fn conflict() -> BackendError {
    BackendError::InvalidOperation(
        "対象は別の操作で変更されています。現在の構成を確認してください".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn delta_preserves_unrelated_changes_and_rejects_conflicts() {
        assert_eq!(
            apply_delta(
                &json!({"x":2,"gain":9}),
                &json!({"x":2,"gain":0}),
                &json!({"x":1,"gain":0})
            )
            .unwrap(),
            json!({"x":1,"gain":9})
        );
        assert!(apply_delta(&json!({"x":3}), &json!({"x":2}), &json!({"x":1})).is_err());
        assert!(apply_delta(
            &json!({}),
            &json!({"node":{"x":1}}),
            &json!({"node":{"x":2}})
        )
        .is_err());
    }
}
