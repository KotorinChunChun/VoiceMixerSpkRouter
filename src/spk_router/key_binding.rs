//! 取得・設定・表示操作のキー設定。保存配列の操作順は変更しない。
use serde::{Deserialize, Serialize};

pub const HOTKEY_COUNT: usize = 6;
pub const HOTKEY_LABELS: [&str; HOTKEY_COUNT] = [
    "再生デバイスへ切替",
    "アプリへ切替",
    "ブラウザへ切替",
    "停止へ切替",
    "設定画面を開く",
    "オーバーレイ表示切替",
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    #[default]
    #[serde(alias = "exclusive")]
    Coexist,
    Override,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    pub enabled: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub win: bool,
    pub alt: bool,
    pub key: String,
    #[serde(default)]
    pub mode: KeyMode,
}

impl Hotkey {
    pub fn is_assigned(&self) -> bool {
        self.enabled && self.key != "None"
    }

    pub fn key_label(&self) -> &str {
        if self.is_assigned() {
            &self.key
        } else {
            "なし"
        }
    }

    pub fn virtual_key(&self) -> Result<u32, String> {
        let value = self.key.as_bytes();
        if value.len() == 1 && value[0].is_ascii_uppercase() {
            return Ok(value[0] as u32);
        }
        if let Some(number) = self
            .key
            .strip_prefix('F')
            .and_then(|v| v.parse::<u32>().ok())
        {
            if (1..=24).contains(&number) && self.key == format!("F{number}") {
                return Ok(0x70 + number - 1);
            }
        }
        Err("キーはA〜ZまたはF1〜F24から選択してください".into())
    }

    pub fn modifiers(&self) -> u32 {
        self.alt as u32
            | ((self.ctrl as u32) << 1)
            | ((self.shift as u32) << 2)
            | ((self.win as u32) << 3)
    }

    pub fn label(&self) -> String {
        if !self.is_assigned() {
            return "割当なし".into();
        }
        let mut labels = Vec::new();
        for (enabled, label) in [
            (self.ctrl, "Ctrl"),
            (self.shift, "Shift"),
            (self.alt, "Alt"),
            (self.win, "Win"),
        ] {
            if enabled {
                labels.push(label);
            }
        }
        labels.push(&self.key);
        labels.join("+")
    }
}

pub fn default_hotkeys() -> [Hotkey; HOTKEY_COUNT] {
    std::array::from_fn(|index| Hotkey {
        enabled: index < 4,
        shift: index < 4,
        ctrl: false,
        win: false,
        alt: false,
        mode: KeyMode::Coexist,
        key: if index < 4 {
            format!("F{}", index + 1)
        } else {
            "None".into()
        },
    })
}

pub fn validate_hotkeys(hotkeys: &[Hotkey; HOTKEY_COUNT]) -> Result<(), String> {
    let mut assigned = Vec::new();
    for key in hotkeys {
        if !key.is_assigned() {
            continue;
        }
        let pair = (key.modifiers(), key.virtual_key()?);
        if assigned.contains(&pair) {
            return Err("同じキーを複数の操作に割り当てられません".into());
        }
        assigned.push(pair);
    }
    Ok(())
}

pub fn key_choices() -> Vec<String> {
    std::iter::once("なし".into())
        .chain((b'A'..=b'Z').map(|c| (c as char).to_string()))
        .chain((1..=24).map(|n| format!("F{n}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_all_mic_key_choices_and_modifier_combinations() {
        let mut key = default_hotkeys()[0].clone();
        for value in key_choices().into_iter().skip(1) {
            key.key = value;
            assert!(key.virtual_key().is_ok());
        }
        key.key = "F24".into();
        for modifiers in 0..16 {
            key.alt = modifiers & 1 != 0;
            key.ctrl = modifiers & 2 != 0;
            key.shift = modifiers & 4 != 0;
            key.win = modifiers & 8 != 0;
            assert_eq!(key.modifiers(), modifiers);
            assert!(key.label().ends_with("F24"));
        }
        for value in ["", "a", "F0", "F25", "F01", "Enter"] {
            key.key = value.into();
            assert!(key.virtual_key().is_err(), "{value}");
        }
    }

    #[test]
    fn none_and_disabled_are_unregistered_but_disabled_selection_is_retained() {
        let mut key = default_hotkeys()[3].clone();
        assert_eq!((key.modifiers(), key.key.as_str()), (4, "F4"));
        key.enabled = false;
        assert!(!key.is_assigned());
        assert_eq!(key.label(), "割当なし");
        assert_eq!(key.key, "F4");
        key.enabled = true;
        key.key = "None".into();
        assert!(!key.is_assigned());
    }

    #[test]
    fn duplicate_assignments_are_rejected_but_disabled_duplicates_are_allowed() {
        let mut keys = default_hotkeys();
        keys[1] = keys[0].clone();
        assert!(validate_hotkeys(&keys).is_err());
        keys[1].enabled = false;
        assert!(validate_hotkeys(&keys).is_ok());
        keys[1].enabled = true;
        keys[1].ctrl = true;
        assert!(validate_hotkeys(&keys).is_ok());
    }
}

pub fn deserialize_hotkeys<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<[Hotkey; HOTKEY_COUNT], D::Error> {
    let values = Vec::<Hotkey>::deserialize(deserializer)?;
    if values.len() != 4 && values.len() != HOTKEY_COUNT {
        return Err(serde::de::Error::custom(
            "キー設定は4行または6行で指定してください",
        ));
    }
    let mut keys = default_hotkeys();
    for (index, key) in values.into_iter().enumerate() {
        keys[index] = key;
    }
    Ok(keys)
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    #[test]
    fn four_rows_keep_custom_and_disabled_values_and_new_actions_have_no_default_keys() {
        let mut original = default_hotkeys();
        original[0].key = "F24".into();
        original[1].enabled = false;
        original[2].key = "None".into();
        let mut json = serde_json::to_value(crate::config::Settings::default()).unwrap();
        json["hotkeys"] = serde_json::to_value(&original[..4]).unwrap();
        let migrated: crate::config::Settings = serde_json::from_value(json).unwrap();
        assert_eq!(migrated.hotkeys[..4], original[..4]);
        assert!(migrated.hotkeys[4..].iter().all(|key| !key.is_assigned()));
        assert_eq!(migrated.outputs.len(), 2);
        assert_eq!(
            HOTKEY_LABELS,
            [
                "再生デバイスへ切替",
                "アプリへ切替",
                "ブラウザへ切替",
                "停止へ切替",
                "設定画面を開く",
                "オーバーレイ表示切替"
            ]
        );
    }

    #[test]
    fn legacy_alt_and_modes_survive_migration_and_round_trip() {
        let mut json = serde_json::to_value(default_hotkeys()[0].clone()).unwrap();
        json.as_object_mut().unwrap().remove("mode");
        json["alt"] = true.into();
        json["shift"] = false.into();
        let mut key: Hotkey = serde_json::from_value(json).unwrap();
        assert_eq!(key.mode, KeyMode::Coexist);
        assert_eq!(key.label(), "Alt+F1");
        key.mode = KeyMode::Override;
        key.ctrl = true;
        key.shift = true;
        key.win = true;
        assert_eq!(key.label(), "Ctrl+Shift+Alt+Win+F1");
        assert_eq!(
            serde_json::from_str::<Hotkey>(&serde_json::to_string(&key).unwrap()).unwrap(),
            key
        );
    }
}
