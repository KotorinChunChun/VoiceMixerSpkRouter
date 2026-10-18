//! OSから取得する再生デバイスと実行中アプリの選択候補。
use audio_mixer_backend::windows::{
    devices::{
        list_devices, DeviceDirection, DEFAULT_COMMUNICATIONS_ENDPOINT, DEFAULT_CONSOLE_ENDPOINT,
    },
    process_watcher::{list_processes, normalize_exe_path},
};
use std::path::{Path, PathBuf};
pub fn device_choices(id: &str, name: &str) -> Result<Vec<(String, String)>, String> {
    let mut choices = vec![
        (String::new(), "未選択".into()),
        (DEFAULT_CONSOLE_ENDPOINT.into(), "既定のデバイス".into()),
        (
            DEFAULT_COMMUNICATIONS_ENDPOINT.into(),
            "既定の通信デバイス".into(),
        ),
    ];
    choices.extend(
        list_devices(DeviceDirection::Render)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|d| (d.endpoint_id, d.friendly_name)),
    );
    if !choices.iter().any(|(value, _)| value == id) {
        choices.push((id.into(), format!("{name}（未接続）")));
    }
    sort_device_choices(&mut choices);
    Ok(choices)
}
pub fn application_choices(current: &Path) -> Vec<(PathBuf, String)> {
    let mut seen = std::collections::HashSet::new();
    let own = std::env::current_exe().ok().map(|p| normalize_exe_path(&p));
    let mut choices: Vec<_> = list_processes()
        .into_iter()
        .filter_map(|p| {
            let path = p.exe_path?;
            let key = normalize_exe_path(&path);
            if own.as_ref() == Some(&key) || !seen.insert(key) {
                return None;
            }
            Some((
                path.clone(),
                format!("{} — {}", p.application_name, path.display()),
            ))
        })
        .collect();
    choices.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    if !current.as_os_str().is_empty()
        && !choices
            .iter()
            .any(|(p, _)| normalize_exe_path(p) == normalize_exe_path(current))
    {
        choices.insert(
            0,
            (current.into(), format!("{}（未起動）", current.display())),
        );
    }
    choices.insert(0, (PathBuf::new(), "未選択".into()));
    choices
}

fn sort_device_choices(choices: &mut [(String, String)]) {
    // 未選択・通常既定・通信既定を固定し、同名は取得順を保持する安定ソート。
    if choices.len() > 3 {
        choices[3..].sort_by_key(|(_, name)| name.to_lowercase());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_sorted_stably_after_special_items_and_ids_are_preserved() {
        let mut choices: Vec<(String, String)> = [
            ("", "未選択"),
            ("default", "既定"),
            ("communications", "通信"),
            ("z", "Zoo"),
            ("a1", "alpha"),
            ("a2", "ALPHA"),
            ("old", "Beta（未接続）"),
        ]
        .map(|(id, n)| (id.into(), n.into()))
        .into();
        sort_device_choices(&mut choices);
        assert_eq!(
            choices.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
            ["", "default", "communications", "a1", "a2", "old", "z"]
        );
    }
}
