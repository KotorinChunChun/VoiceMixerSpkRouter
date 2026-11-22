//! 開発用の隔離セッション。OS音声・常駐IPCには接続しない。
use crate::{
    config::{Mode, OverlayMode, Settings},
    engine::{
        commit_audio,
        routing::{plan_route, same_endpoint, Route, OUTPUT_FIELDS},
    },
    key_binding::{Hotkey, HOTKEY_COUNT},
    state::Phase,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
};
const MAX_LINE: usize = 64 * 1024;
const OWNER: &str = "owner";
const SETTINGS: &str = "settings.json";

struct Workspace {
    id: String,
    path: PathBuf,
}
impl Workspace {
    fn new() -> Result<Self, String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir();
        if !root.is_absolute() {
            return Err("隔離テストの一時領域が不正です".into());
        }
        let path = root.join(format!("VoiceMixerSpkRouter-test-{id}"));
        fs::create_dir(&path).map_err(|_| "隔離テストの一時領域を作成できません")?;
        if fs::write(path.join(OWNER), &id).is_err() {
            let _ = fs::remove_file(path.join(OWNER));
            let _ = fs::remove_dir(&path);
            return Err("隔離テストの所有情報を作成できません".into());
        }
        Ok(Self { id, path })
    }
    fn config(&self) -> PathBuf {
        self.path.join(SETTINGS)
    }
}
#[cfg(windows)]
fn ordinary(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 == 0 // FILE_ATTRIBUTE_REPARSE_POINT
}
#[cfg(not(windows))]
fn ordinary(metadata: &fs::Metadata) -> bool {
    !metadata.file_type().is_symlink()
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let marker = self.path.join(OWNER);
        if !fs::symlink_metadata(&self.path).is_ok_and(|m| m.is_dir() && ordinary(&m))
            || !fs::symlink_metadata(&marker)
                .is_ok_and(|m| m.is_file() && ordinary(&m) && m.len() == 32)
            || fs::read_to_string(&marker).ok().as_deref() != Some(&self.id)
        {
            return;
        }
        // 所有を確認した領域の、既知の試験ファイルだけを削除する。
        let config = self.config();
        for path in [
            &config,
            &config.with_extension(format!("{}.tmp", std::process::id())),
            &marker,
        ] {
            if fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && ordinary(&m)) {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_dir(&self.path);
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FixtureId {
    Default,
    Communications,
    Speaker,
    SpeakerAlias,
    Headphones,
    Cable,
    Disconnected,
    Unselected,
}
impl FixtureId {
    fn id(self) -> &'static str {
        match self {
            Self::Default => "default://console",
            Self::Communications => "default://communications",
            Self::Speaker => "speaker",
            Self::SpeakerAlias => "speaker-alias",
            Self::Headphones => "headphones",
            Self::Cable => "cable",
            Self::Disconnected => "disconnected",
            Self::Unselected => "",
        }
    }
}
fn resolve(id: &str) -> Result<String, String> {
    match id {
        "default://console" | "speaker" | "speaker-alias" => Ok("speaker".into()),
        "default://communications" | "headphones" => Ok("headphones".into()),
        "cable" => Ok("cable".into()),
        _ => Err("試験デバイスを解決できません".into()),
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutputPatch {
    device_id: Option<FixtureId>,
    enabled: Option<bool>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Patch {
    mode: Option<Mode>,
    input_enabled: Option<[bool; 3]>,
    input_device_id: Option<FixtureId>,
    outputs: Option<[Option<OutputPatch>; 2]>,
    browser_port: Option<u16>,
    overlay_mode: Option<OverlayMode>,
    hotkeys: Option<[Option<Hotkey>; HOTKEY_COUNT]>,
}
impl Patch {
    fn belongs_to(&self, page: SettingsPage) -> bool {
        let basic = self.input_enabled.is_some()
            || self.input_device_id.is_some()
            || self.outputs.is_some()
            || self.browser_port.is_some()
            || self.overlay_mode.is_some();
        self.mode.is_none()
            && match page {
                SettingsPage::Basic => self.hotkeys.is_none(),
                SettingsPage::Keys => !basic,
            }
    }

    fn candidate(self, current: &Settings, mode: Option<Mode>) -> Settings {
        let mut next = current.clone();
        if let Some(mode) = mode.or(self.mode) {
            next.mode = mode;
        }
        if let Some(flags) = self.input_enabled {
            next.input_enabled = flags;
        }
        if let Some(id) = self.input_device_id {
            next.device_id = id.id().into();
        }
        if let Some(port) = self.browser_port {
            next.browser_port = port;
        }
        if let Some(mode) = self.overlay_mode {
            next.overlay_mode = mode;
        }
        for (current, key) in next
            .hotkeys
            .iter_mut()
            .zip(self.hotkeys.unwrap_or_default())
        {
            if let Some(key) = key {
                *current = key;
            }
        }
        for (i, patch) in self.outputs.unwrap_or_default().into_iter().enumerate() {
            if let Some(patch) = patch {
                if let Some(id) = patch.device_id {
                    next.outputs[i].endpoint_id = id.id().into();
                }
                if let Some(enabled) = patch.enabled {
                    next.outputs[i].enabled = enabled;
                }
            }
        }
        next
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Fault {
    #[default]
    None,
    Audio,
    Save,
    Rollback,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    schema_version: u32,
    request_id: String,
    command: String,
    expected_revision: Option<u64>,
    output: Option<usize>,
    enabled: Option<bool>,
    mode: Option<Mode>,
    patch: Option<Patch>,
    fault: Option<Fault>,
    page: Option<SettingsPage>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SettingsPage {
    Basic,
    Keys,
}

struct SettingsEditor {
    page: SettingsPage,
    base_revision: u64,
    base: Settings,
    draft: Settings,
}

#[derive(Serialize)]
struct Issue {
    code: &'static str,
    field: &'static str,
    severity: &'static str,
}
fn issue(code: &'static str, field: &'static str, severity: &'static str) -> Issue {
    Issue {
        code,
        field,
        severity,
    }
}

fn assess(next: &Settings) -> (Option<Route>, Vec<Issue>) {
    let mut issues = Vec::new();
    let mut inspection = next.clone();
    inspection.mode = next.available_mode(next.mode);
    let route = match plan_route(&inspection, None, resolve) {
        Ok(route) => Some(route),
        Err(error) => {
            issues.push(issue(error.code, error.field, "error"));
            None
        }
    };
    // 未使用の経路は実行時の拒否対象にせず、候補として予告する。
    let source = next.input_enabled[0]
        .then(|| resolve(&next.device_id).ok())
        .flatten();
    let destinations = next
        .outputs
        .each_ref()
        .map(|output| resolve(&output.endpoint_id).ok());
    if next.input_enabled[0] && source.is_none() && inspection.mode != Mode::Device {
        issues.push(issue("device_unavailable", "inputDeviceId", "warning"));
    }
    for (i, output) in next.outputs.iter().enumerate() {
        if !output.enabled || inspection.mode == Mode::Stopped {
            if destinations[i].is_none() && !output.endpoint_id.is_empty() {
                issues.push(issue("device_unavailable", OUTPUT_FIELDS[i], "warning"));
            }
        }
        if (inspection.mode != Mode::Device || !output.enabled)
            && same_endpoint(source.as_deref(), destinations[i].as_deref())
        {
            issues.push(issue("route_loop", OUTPUT_FIELDS[i], "warning"));
        }
    }
    if route.is_some()
        && next.outputs.iter().all(|o| o.enabled)
        && same_endpoint(destinations[0].as_deref(), destinations[1].as_deref())
    {
        issues.push(issue("output_1_preferred", OUTPUT_FIELDS[1], "warning"));
    }
    (route, issues)
}

struct Session {
    workspace: Workspace,
    settings: Settings,
    effective: Option<Route>,
    revision: u64,
    editor: Option<SettingsEditor>,
}
impl Session {
    fn new() -> Result<Self, String> {
        let workspace = Workspace::new()?;
        let mut settings = Settings::defaults_with_token("fixture-only".into());
        // これは経路モデルの識別子で、実際に起動・読み込みはしない。
        settings.application_path = workspace.path.join("fixture-application.exe");
        settings
            .save(&workspace.config())
            .map_err(|_| "隔離設定を初期化できません")?;
        let effective =
            plan_route(&settings, None, resolve).map_err(|_| "隔離経路を初期化できません")?;
        Ok(Self {
            workspace,
            settings,
            effective: Some(effective),
            revision: 0,
            editor: None,
        })
    }
    fn state(&self) -> Value {
        let phase = match (&self.effective, self.settings.mode) {
            (None, _) => Phase::Error,
            (_, Mode::Stopped) => Phase::Stopped,
            (_, Mode::Device) => Phase::Running,
            (_, Mode::Application | Mode::Browser) => Phase::Waiting,
        };
        json!({
            "mode": self.settings.mode,
            "phase": phase,
            "inputEnabled": self.settings.input_enabled,
            "outputsEnabled": self.settings.outputs.each_ref().map(|o| o.enabled),
            "effectiveOutputs": self.effective.as_ref().map(|r| &r.outputs),
            "browserPort": self.settings.browser_port,
            "overlayMode": self.settings.overlay_mode,
            "hotkeys": self.settings.hotkeys,
            "settingsEditor": self.editor.as_ref().map(|editor| json!({
                "page": editor.page,
                "baseRevision": editor.base_revision,
                "dirty": editor.draft != editor.base,
            })),
            "simulated": true
        })
    }
    fn reply(
        &self,
        id: Option<&str>,
        command: Option<&str>,
        result: &str,
        issues: &[Issue],
    ) -> Value {
        json!({
            "schemaVersion": 1, "sessionId": self.workspace.id, "requestId": id,
            "command": command, "result": result, "revision": self.revision,
            "issues": issues, "state": self.state()
        })
    }
    fn reject(
        &self,
        id: Option<&str>,
        command: Option<&str>,
        code: &'static str,
        field: &'static str,
    ) -> Value {
        self.reply(id, command, "rejected", &[issue(code, field, "error")])
    }
    fn handle(&mut self, line: &[u8]) -> (Value, bool) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return (self.reject(None, None, "invalid_json", "request"), false);
        };
        let Some(object) = value.as_object() else {
            return (self.reject(None, None, "invalid_request", "request"), false);
        };
        let id = value["requestId"].as_str().filter(|id| {
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
        let command = match value["command"].as_str() {
            Some(
                command @ ("status"
                | "validate-route"
                | "set-output"
                | "apply"
                | "shutdown"
                | "open-settings"
                | "select-settings-page"
                | "edit-settings"
                | "apply-settings"
                | "close-settings"),
            ) => command,
            _ => return (self.reject(id, None, "unknown_command", "command"), false),
        };
        let fields: &[&str] = match command {
            "validate-route" => &["mode", "patch"],
            "set-output" => &["expectedRevision", "output", "enabled", "fault"],
            "apply" => &["expectedRevision", "patch", "fault"],
            "select-settings-page" => &["page"],
            "edit-settings" => &["patch"],
            "apply-settings" => &["expectedRevision", "fault"],
            _ => &[],
        };
        if object.keys().any(|k| {
            !matches!(k.as_str(), "schemaVersion" | "requestId" | "command")
                && !fields.contains(&k.as_str())
        }) {
            return (
                self.reject(id, Some(command), "unknown_field", "request"),
                false,
            );
        }
        let unknown_patch = value["patch"].as_object().is_some_and(|patch| {
            patch.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "mode"
                        | "inputEnabled"
                        | "inputDeviceId"
                        | "outputs"
                        | "browserPort"
                        | "overlayMode"
                        | "hotkeys"
                )
            }) || patch
                .get("outputs")
                .and_then(Value::as_array)
                .is_some_and(|outputs| {
                    outputs.iter().filter_map(Value::as_object).any(|out| {
                        out.keys()
                            .any(|k| !matches!(k.as_str(), "deviceId" | "enabled"))
                    })
                })
        });
        if unknown_patch {
            return (
                self.reject(id, Some(command), "unknown_field", "patch"),
                false,
            );
        }
        let Ok(request) = serde_json::from_slice::<Request>(line) else {
            return (
                self.reject(id, Some(command), "invalid_request", "request"),
                false,
            );
        };
        if request.schema_version != 1 {
            return (
                self.reject(id, Some(command), "unsupported_schema", "schemaVersion"),
                false,
            );
        }
        if id.is_none() {
            return (
                self.reject(None, Some(command), "invalid_request_id", "requestId"),
                false,
            );
        }
        let id = Some(request.request_id.as_str());
        let command = Some(request.command.as_str());
        let reply = |this: &Self, result, issues: &[Issue]| this.reply(id, command, result, issues);
        let reject = |this: &Self, code, field| this.reject(id, command, code, field);
        if request
            .patch
            .as_ref()
            .and_then(|patch| patch.hotkeys.as_ref())
            .is_some_and(|keys| {
                keys.iter().flatten().any(|key| {
                    if key.key == "None" {
                        key.enabled
                    } else {
                        key.virtual_key().is_err()
                    }
                })
            })
        {
            return (reject(self, "invalid_hotkey", "patch.hotkeys"), false);
        }
        let next = match command {
            Some("status") => return (reply(self, "ok", &[]), false),
            Some("shutdown") => return (reply(self, "ok", &[]), true),
            Some("open-settings") => {
                self.editor.get_or_insert_with(|| SettingsEditor {
                    page: SettingsPage::Basic,
                    base_revision: self.revision,
                    base: self.settings.clone(),
                    draft: self.settings.clone(),
                });
                return (reply(self, "ok", &[]), false);
            }
            Some("close-settings") => {
                self.editor = None;
                return (reply(self, "ok", &[]), false);
            }
            Some("select-settings-page" | "edit-settings") => {
                let Some(editor) = self.editor.as_mut() else {
                    return (reject(self, "settings_closed", "settingsEditor"), false);
                };
                if command == Some("select-settings-page") {
                    let Some(page) = request.page else {
                        return (reject(self, "invalid_request", "page"), false);
                    };
                    editor.page = page;
                } else {
                    let Some(patch) = request.patch else {
                        return (reject(self, "invalid_request", "patch"), false);
                    };
                    if !patch.belongs_to(editor.page) {
                        return (reject(self, "wrong_settings_page", "patch"), false);
                    }
                    editor.draft = patch.candidate(&editor.draft, None);
                }
                return (reply(self, "ok", &[]), false);
            }
            Some("validate-route") => {
                let Some(mode) = request.mode else {
                    return (reject(self, "invalid_request", "mode"), false);
                };
                let patch = request.patch.unwrap_or_default();
                if patch.mode.is_some() {
                    return (reject(self, "unexpected_field", "patch.mode"), false);
                }
                let next = patch.candidate(&self.settings, Some(mode));
                let (route, issues) = assess(&next);
                let mut response = reply(
                    self,
                    if route.is_some() { "valid" } else { "rejected" },
                    &issues,
                );
                response["candidate"] = json!({
                    "mode": next.available_mode(next.mode),
                    "outputsEnabled": next.outputs.each_ref().map(|o| o.enabled),
                    "effectiveOutputs": route.map(|r| r.outputs),
                    "resolvedInputDevice": next.input_enabled[0].then(|| resolve(&next.device_id).ok()).flatten(),
                    "resolvedOutputDevices": next.outputs.each_ref().map(|o| resolve(&o.endpoint_id).ok())
                });
                return (response, false);
            }
            Some("apply" | "set-output" | "apply-settings") => {
                match request.expected_revision {
                    None => return (reject(self, "invalid_request", "expectedRevision"), false),
                    Some(n) if n != self.revision => {
                        return (reject(self, "stale_revision", "expectedRevision"), false)
                    }
                    _ => {}
                }
                if command == Some("apply-settings") {
                    let Some(editor) = self.editor.as_ref() else {
                        return (reject(self, "settings_closed", "settingsEditor"), false);
                    };
                    if editor.base_revision != self.revision {
                        return (
                            reject(self, "stale_settings_draft", "settingsEditor"),
                            false,
                        );
                    }
                    editor.draft.clone()
                } else if command == Some("apply") {
                    let Some(patch) = request.patch else {
                        return (reject(self, "invalid_request", "patch"), false);
                    };
                    patch.candidate(&self.settings, None)
                } else {
                    let (Some(output @ 1..=2), Some(enabled)) = (request.output, request.enabled)
                    else {
                        return (reject(self, "invalid_output", "output"), false);
                    };
                    let mut next = self.settings.clone();
                    next.outputs[output - 1].enabled = enabled;
                    next
                }
            }
            _ => unreachable!("コマンド名は事前検証済み"),
        };
        let (route, issues) = assess(&next);
        let Some(route) = route else {
            return (reply(self, "rejected", &issues), false);
        };
        let mut next = next;
        next.mode = next.available_mode(next.mode);
        let fault = request.fault.unwrap_or_default();
        let changed = next != self.settings;
        if !changed && self.effective.is_some() {
            return (reply(self, "unchanged", &issues), false);
        }
        let previous = self.effective.clone();
        let result = commit_audio(
            &mut self.effective,
            &mut self.settings,
            next,
            |active, _| {
                *active = Some(route);
                if fault == Fault::Audio {
                    Err("試験用の音声反映失敗".into())
                } else {
                    Ok(())
                }
            },
            |next| {
                if matches!(fault, Fault::Save | Fault::Rollback) {
                    return Err("試験用の保存失敗".into());
                }
                next.save(&self.workspace.config())
            },
            |active, _| {
                if fault == Fault::Rollback {
                    *active = None;
                    Err("試験用の復旧失敗".into())
                } else {
                    *active = previous;
                    Ok(())
                }
            },
        );
        match result {
            Ok(()) => {
                if changed {
                    self.revision += 1;
                }
                if command == Some("apply-settings") {
                    if let Some(editor) = self.editor.as_mut() {
                        editor.base_revision = self.revision;
                        editor.base = self.settings.clone();
                        editor.draft = self.settings.clone();
                    }
                }
                (reply(self, "applied", &issues), false)
            }
            Err(error) => (
                reply(
                    self,
                    "failed",
                    &[issue(
                        if error.recovery.is_some() {
                            "rollback_failed"
                        } else {
                            error.stage
                        },
                        "settings",
                        "error",
                    )],
                ),
                false,
            ),
        }
    }
}

// 改行がない巨大な入力も全量を保持せず、次の行まで読み捨てる。
fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    line.clear();
    let mut received = false;
    let mut overlong = false;
    loop {
        let chunk = input.fill_buf()?;
        if chunk.is_empty() {
            return Ok(received.then_some(overlong));
        }
        received = true;
        let newline = chunk.iter().position(|&b| b == b'\n');
        let len = newline.unwrap_or(chunk.len());
        if line.len() + len > MAX_LINE {
            overlong = true;
        }
        if !overlong {
            line.extend_from_slice(&chunk[..len]);
        }
        input.consume(len + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(overlong));
        }
    }
}

pub fn run() -> Result<(), String> {
    let mut session = Session::new()?;
    let input = io::stdin();
    let output = io::stdout();
    let mut input = input.lock();
    let mut output = output.lock();
    let mut write = |value: Value| {
        serde_json::to_writer(&mut output, &value)
            .map_err(|_| "隔離テスト応答の出力に失敗しました")?;
        writeln!(output)
            .and_then(|_| output.flush())
            .map_err(|_| "隔離テスト応答の出力に失敗しました")
    };
    let mut ready = session.reply(None, None, "ok", &[]);
    ready["event"] = json!("ready");
    write(ready)?;
    let mut line = Vec::new();
    while let Some(overlong) =
        read_line(&mut input, &mut line).map_err(|_| "隔離テスト入力に失敗しました")?
    {
        if overlong {
            write(session.reject(None, None, "line_too_long", "request"))?;
            continue;
        }
        let (reply, shutdown) = session.handle(&line);
        write(reply)?;
        if shutdown {
            break;
        }
    }
    Ok(())
}
