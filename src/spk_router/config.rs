//! 永続化する設定と検証。OSの候補列挙はcatalogに分離する。
use crate::key_binding::{default_hotkeys, validate_hotkeys, Hotkey, HOTKEY_COUNT};
use audio_mixer_backend::windows::devices::DEFAULT_CONSOLE_ENDPOINT;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const APP_VERSION: &str = "1.0.11";
pub const APP_NAME: &str = "VoiceMixerSpkRouter";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Device,
    Application,
    Browser,
    #[default]
    Stopped,
}
impl Mode {
    pub fn input_index(self) -> Option<usize> {
        match self {
            Self::Device => Some(0),
            Self::Application => Some(1),
            Self::Browser => Some(2),
            Self::Stopped => None,
        }
    }
    #[cfg(test)]
    pub const ALL: [Self; 4] = [
        Self::Device,
        Self::Application,
        Self::Browser,
        Self::Stopped,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Device => "再生デバイス",
            Self::Application => "アプリ",
            Self::Browser => "ブラウザ",
            Self::Stopped => "停止",
        }
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "device" => Ok(Self::Device),
            "application" | "app" => Ok(Self::Application),
            "browser" => Ok(Self::Browser),
            "stopped" | "stop" => Ok(Self::Stopped),
            _ => Err(format!("不明な取得モード: {value}")),
        }
    }
    pub fn toggle(self, selected: Self) -> Self {
        if self == selected {
            Self::Stopped
        } else {
            selected
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayMode {
    Off,
    Brief,
    #[default]
    Always,
}
impl OverlayMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Brief,
            Self::Brief => Self::Always,
            Self::Always => Self::Off,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "オフ",
            Self::Brief => "数秒表示",
            Self::Always => "常時表示",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub endpoint_id: String,
    pub name: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    pub input_enabled: [bool; 3],
    #[serde(
        default = "default_hotkeys",
        deserialize_with = "crate::key_binding::deserialize_hotkeys"
    )]
    pub hotkeys: [Hotkey; HOTKEY_COUNT],
    pub device_id: String,
    pub device_name: String,
    pub application_path: PathBuf,
    pub outputs: [Output; 2],
    pub browser_port: u16,
    pub browser_token: String,
    pub browser_stream_key: String,
    pub overlay_mode: OverlayMode,
    pub startup_mode: Option<Mode>,
    pub output_labels: [String; 2],
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Stopped,
            input_enabled: [true; 3],
            hotkeys: default_hotkeys(),
            device_id: DEFAULT_CONSOLE_ENDPOINT.into(),
            device_name: "既定のデバイス".into(),
            application_path: PathBuf::new(),
            outputs: std::array::from_fn(|_| Output {
                endpoint_id: String::new(),
                name: "未選択".into(),
                enabled: false,
            }),
            browser_port: 18766,
            browser_token: uuid::Uuid::new_v4().to_string(),
            browser_stream_key: "spk-router".into(),
            overlay_mode: OverlayMode::Always,
            startup_mode: None,
            output_labels: ["出力1".into(), "出力2".into()],
        }
    }
}
impl Settings {
    pub fn mode_enabled(&self, mode: Mode) -> bool {
        mode.input_index().is_none_or(|i| self.input_enabled[i])
    }
    pub fn available_mode(&self, mode: Mode) -> Mode {
        if self.mode_enabled(mode) {
            mode
        } else {
            Mode::Stopped
        }
    }
    /// 表示とキーだけの変更では、取得・転送の状態を切り替えない。
    pub fn requires_audio_update(&self, next: &Self) -> bool {
        self.mode != next.mode
            || self.input_enabled[2] != next.input_enabled[2]
            || self
                .mode
                .input_index()
                .is_some_and(|i| self.input_enabled[i] != next.input_enabled[i])
            || self.device_id != next.device_id
            || self.application_path != next.application_path
            || self.browser_port != next.browser_port
            || self.browser_token != next.browser_token
            || self.browser_stream_key != next.browser_stream_key
            || self
                .outputs
                .iter()
                .zip(&next.outputs)
                .any(|(current, next)| {
                    current.enabled != next.enabled || current.endpoint_id != next.endpoint_id
                })
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_hotkeys(&self.hotkeys)?;
        for label in &self.output_labels {
            if label.trim() != label
                || !(1..=24).contains(&label.chars().count())
                || label.chars().any(char::is_control)
            {
                return Err("用途名は前後空白のない1〜24文字で指定してください".into());
            }
        }
        if self.device_id.trim().is_empty() {
            return Err("取得する再生デバイスを選択してください".into());
        }
        if !self.application_path.as_os_str().is_empty() && !self.application_path.is_absolute() {
            return Err("アプリは実行ファイルの絶対パスを指定してください".into());
        }
        if self.browser_port == 0
            || self.browser_token.trim().is_empty()
            || self.browser_stream_key.trim().is_empty()
        {
            return Err("ブラウザのポート・認証トークン・ストリームキーを指定してください".into());
        }
        for output in &self.outputs {
            if output.enabled && output.endpoint_id.is_empty() {
                return Err("ONにする出力の再生デバイスを選択してください".into());
            }
        }
        if self.outputs.iter().all(|o| o.enabled)
            && self.outputs[0].endpoint_id == self.outputs[1].endpoint_id
        {
            return Err("出力1と2には異なる再生デバイスを指定してください".into());
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if let Some(object) = json.as_object_mut() {
            if !object.contains_key("overlay_mode") {
                let visible = object
                    .get("overlay")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                object.insert(
                    "overlay_mode".into(),
                    serde_json::json!(if visible { "always" } else { "off" }),
                );
            }
            object.remove("overlay");
        }
        let value: Self = serde_json::from_value(json).map_err(|e| e.to_string())?;
        value.validate()?;
        Ok(value)
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        write_json(path, self)
    }
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_is_exclusive_and_repeated_key_stops() {
        for current in Mode::ALL {
            for selected in Mode::ALL {
                assert_eq!(
                    current.toggle(selected),
                    if current == selected {
                        Mode::Stopped
                    } else {
                        selected
                    }
                );
            }
        }
    }
    #[test]
    fn defaults_do_not_capture_or_play() {
        let settings = Settings::default();
        assert_eq!(settings.mode, Mode::Stopped);
        assert!(settings.outputs.iter().all(|o| !o.enabled));
        assert_eq!(settings.input_enabled, [true; 3]);
        settings.validate().unwrap();
    }
    #[test]
    fn old_settings_enable_all_sources_and_disabled_flags_roundtrip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let original = Settings::default();
        let mut old = serde_json::to_value(&original).unwrap();
        old.as_object_mut().unwrap().remove("input_enabled");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), original);
        let mut next = original.clone();
        next.input_enabled = [false, true, false];
        next.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), next);
    }
    #[test]
    fn output_selection_and_duplicate_validation() {
        let mut s = Settings::default();
        s.outputs[0].enabled = true;
        assert!(s.validate().is_err());
        s.outputs[0].endpoint_id = "one".into();
        s.validate().unwrap();
        s.outputs[1] = s.outputs[0].clone();
        assert!(s.validate().is_err());
        s.outputs[1].enabled = false;
        s.validate().unwrap();
    }
    #[test]
    fn settings_survive_atomic_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spk-settings.json");
        let mut s = Settings::default();
        s.save(&path).unwrap();
        s.mode = Mode::Browser;
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
    }

    #[test]
    fn old_settings_gain_default_keys_without_changing_saved_audio_configuration() {
        let mut original = Settings::default();
        original.mode = Mode::Browser;
        original.device_id = "saved-device".into();
        original.browser_token = "saved-test-token".into();
        let mut json = serde_json::to_value(&original).unwrap();
        json.as_object_mut().unwrap().remove("hotkeys");
        let migrated: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(migrated, original);
        assert_eq!(migrated.hotkeys, default_hotkeys());
    }

    #[test]
    fn custom_keys_and_disabled_selection_survive_save_and_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("spk-settings.json");
        let mut settings = Settings::default();
        settings.hotkeys[0].key = "F24".into();
        settings.hotkeys[0].ctrl = true;
        settings.hotkeys[1].enabled = false;
        settings.hotkeys[2].key = "None".into();
        settings.hotkeys[2].enabled = false;
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
    }

    #[test]
    fn keys_and_display_only_do_not_reconfigure_audio_but_route_changes_do() {
        let current = Settings::default();
        let mut next = current.clone();
        next.hotkeys[0].ctrl = true;
        next.hotkeys[3].enabled = false;
        next.overlay_mode = OverlayMode::Off;
        next.startup_mode = Some(Mode::Browser);
        next.output_labels[0] = "配信用".into();
        next.device_name = "表示名だけ".into();
        next.outputs[0].name = "出力名だけ".into();
        assert!(!current.requires_audio_update(&next));
        let changes: [fn(&mut Settings); 8] = [
            |value: &mut Settings| value.mode = Mode::Browser,
            |value: &mut Settings| value.device_id = "other-device".into(),
            |value: &mut Settings| value.application_path = "C:/test/application.exe".into(),
            |value: &mut Settings| value.outputs[0].enabled = true,
            |value: &mut Settings| value.outputs[1].endpoint_id = "another-output".into(),
            |value: &mut Settings| value.browser_port += 1,
            |value: &mut Settings| value.browser_token.push('x'),
            |value: &mut Settings| value.browser_stream_key.push('x'),
        ];
        for change in changes {
            let mut next = current.clone();
            change(&mut next);
            assert!(current.requires_audio_update(&next));
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn legacy_overlay_and_new_precedence_preserve_saved_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for (legacy, mode) in [(true, OverlayMode::Always), (false, OverlayMode::Off)] {
            let original = Settings::default();
            let mut json = serde_json::to_value(&original).unwrap();
            json.as_object_mut().unwrap().remove("overlay_mode");
            json["overlay"] = serde_json::json!(legacy);
            fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
            let migrated = Settings::load(&path).unwrap();
            assert_eq!(migrated.overlay_mode, mode);
            assert_eq!(migrated.browser_token, original.browser_token);
            assert_eq!(migrated.outputs, original.outputs);
            json["overlay_mode"] = serde_json::json!("brief");
            fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
            let migrated = Settings::load(&path).unwrap();
            assert_eq!(migrated.overlay_mode, OverlayMode::Brief);
            migrated.save(&path).unwrap();
            assert!(!fs::read_to_string(&path).unwrap().contains("\"overlay\":"));
        }
    }
    #[test]
    fn labels_and_display_cycle_are_validated_without_route_changes() {
        let original = Settings::default();
        let mut changed = original.clone();
        changed.output_labels = ["音声入力用".into(), "配信用".into()];
        changed.startup_mode = Some(Mode::Stopped);
        assert!(changed.validate().is_ok());
        assert!(!original.requires_audio_update(&changed));
        for bad in ["", " 名前", "改\n行", "1234567890123456789012345"] {
            changed.output_labels[0] = bad.into();
            assert!(changed.validate().is_err());
        }
        assert_eq!(OverlayMode::Off.next().next().next(), OverlayMode::Off);
    }
}
