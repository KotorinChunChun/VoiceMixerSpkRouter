use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{audio::graph::NodeId, event_graph::EventId};

macro_rules! action_id_type {
    ($name:ident) => {
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
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

action_id_type!(ActionExecutionId);
action_id_type!(ObsConnectionId);
action_id_type!(SecretId);

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
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HttpHeaderConfig {
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub secret_id: Option<SecretId>,
}

impl HttpHeaderConfig {
    pub fn is_sensitive(&self) -> bool {
        matches!(
            self.name.to_ascii_lowercase().as_str(),
            "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HttpActionConfig {
    pub method: HttpMethod,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<HttpHeaderConfig>,
    #[serde(default)]
    pub body_template: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObsActionConfig {
    pub connection_id: ObsConnectionId,
    pub scene_name: String,
}

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
pub struct KeyCode(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyTarget {
    Global,
    ForegroundApplication,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HotkeyActionConfig {
    pub keys: Vec<KeyCode>,
    pub target: HotkeyTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionConfig {
    Obs(ObsActionConfig),
    Http(HttpActionConfig),
    Hotkey(HotkeyActionConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActionNodeConfig {
    pub enabled: bool,
    pub action: ActionConfig,
    pub timeout_ms: u64,
}

impl ActionNodeConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        validate_timeout(self.timeout_ms)?;
        match &self.action {
            ActionConfig::Http(config) => config.validate(),
            ActionConfig::Obs(config) => config.validate(),
            ActionConfig::Hotkey(config) => config.validate(),
        }
    }
}

impl HttpActionConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        validate_timeout(self.timeout_ms)?;
        let url = reqwest::Url::parse(&self.url).map_err(|_| "HTTP URL is invalid".to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("HTTP URL scheme must be http or https".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("HTTP URL must not contain embedded credentials".into());
        }
        if self.headers.len() > 64 {
            return Err("HTTP action supports at most 64 headers".into());
        }
        for header in &self.headers {
            if header.name.trim().is_empty() || header.name.contains(['\r', '\n']) {
                return Err("HTTP header name is invalid".into());
            }
            if header.value.is_some() == header.secret_id.is_some() {
                return Err("HTTP header requires exactly one value or secret reference".into());
            }
            if header.is_sensitive() && header.value.is_some() {
                return Err("sensitive HTTP headers must use a secret reference".into());
            }
            if header
                .value
                .as_deref()
                .is_some_and(|value| value.contains(['\r', '\n']))
            {
                return Err("HTTP header value is invalid".into());
            }
        }
        if self
            .body_template
            .as_ref()
            .is_some_and(|body| body.len() > 1_048_576)
        {
            return Err("HTTP body template must not exceed 1 MiB".into());
        }
        Ok(())
    }
}

impl ObsActionConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.scene_name.trim().is_empty() || self.scene_name.len() > 512 {
            return Err("OBS scene name must contain 1 to 512 characters".into());
        }
        Ok(())
    }
}

impl HotkeyActionConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.keys.is_empty() || self.keys.len() > 8 {
            return Err("hotkey requires between 1 and 8 keys".into());
        }
        if self.keys.iter().any(|key| key.0 == 0) {
            return Err("hotkey key codes must be non-zero".into());
        }
        let unique: std::collections::HashSet<_> = self.keys.iter().map(|key| key.0).collect();
        if unique.len() != self.keys.len() {
            return Err("hotkey must not contain duplicate key codes".into());
        }
        Ok(())
    }
}

fn validate_timeout(timeout_ms: u64) -> std::result::Result<(), String> {
    if !(1..=120_000).contains(&timeout_ms) {
        return Err("action timeout must be between 1 and 120000 ms".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObsConnectionConfig {
    pub id: ObsConnectionId,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub password_secret_id: Option<SecretId>,
}

impl ObsConnectionConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.host.trim().is_empty()
            || self.host.contains(['/', '\\', '\r', '\n'])
            || self.port == 0
        {
            return Err("OBS host or port is invalid".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogOutputFormat {
    PlainText,
    JsonLines,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogOutputConfig {
    pub path: PathBuf,
    pub format: LogOutputFormat,
}

impl LogOutputConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("log output path must not be empty".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ActionExecutionResult {
    pub execution_id: ActionExecutionId,
    pub node_id: NodeId,
    pub event_id: Option<EventId>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub success: bool,
    pub message: Option<String>,
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ActionRuntimeState {
    pub connection_state: ActionConnectionState,
    pub running: bool,
    pub last_result: Option<ActionExecutionResult>,
}

impl Default for ActionRuntimeState {
    fn default() -> Self {
        Self {
            connection_state: ActionConnectionState::Disconnected,
            running: false,
            last_result: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionRuntimeEvent {
    ExecutionStarted {
        execution_id: ActionExecutionId,
        node_id: NodeId,
        event_id: Option<EventId>,
        started_at: DateTime<Utc>,
    },
    ExecutionFinished {
        result: ActionExecutionResult,
    },
    ConnectionStateChanged {
        node_id: NodeId,
        state: ActionConnectionState,
    },
}
