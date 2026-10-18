use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{actions::model::KeyCode, audio::graph::NodeId};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    Ord,
    PartialOrd,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ShortcutId(pub Uuid);

impl ShortcutId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ShortcutId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct NodeActionDescriptor {
    pub action_id: String,
    pub display_name: String,
    pub parameter_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KeyCombination {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub key: KeyCode,
}

impl KeyCombination {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.key.0 == 0 {
            return Err("shortcut key code must be non-zero".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ShortcutBinding {
    pub id: ShortcutId,
    pub enabled: bool,
    pub key_combo: KeyCombination,
    pub node_id: NodeId,
    pub action_id: String,
    #[serde(default)]
    pub action_parameter: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutConflict {
    None,
    DuplicateInVoiceMixer,
    ReservedByVoiceMixer,
    RegistrationFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ShortcutValidation {
    pub conflict: ShortcutConflict,
    pub message: Option<String>,
}

impl ShortcutValidation {
    pub fn none() -> Self {
        Self {
            conflict: ShortcutConflict::None,
            message: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShortcutRegistrationState {
    Unregistered,
    Registered,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShortcutRuntimeEvent {
    ShortcutsChanged {
        bindings: Vec<ShortcutBinding>,
    },
    ShortcutTriggered {
        shortcut_id: ShortcutId,
        node_id: NodeId,
        action_id: String,
        timestamp_utc: DateTime<Utc>,
        success: bool,
        message: Option<String>,
    },
    RegistrationStateChanged {
        shortcut_id: ShortcutId,
        state: ShortcutRegistrationState,
    },
}

pub fn default_reserved_combinations() -> Vec<KeyCombination> {
    // Reserved for VoiceMixer's emergency diagnostic shortcut. Keeping it out
    // of user bindings prevents a future internal registration collision.
    vec![KeyCombination {
        ctrl: true,
        shift: true,
        alt: true,
        win: true,
        key: KeyCode(0x7B), // F12
    }]
}
