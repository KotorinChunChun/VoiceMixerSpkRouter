use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audio::graph::{CacheId, ReplayItemId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    Disabled,
    Ready,
    WaitingForInput,
    Recording,
    Stopping,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordingOutput {
    pub path: PathBuf,
    pub cache_id: Option<CacheId>,
    pub cache_item_id: Option<ReplayItemId>,
    pub created_unix_ms: u64,
    pub duration_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecorderStateSnapshot {
    pub state: RecorderState,
    pub auto_record: bool,
    pub session_active: bool,
    pub input_available: bool,
    pub manual_stop_override: bool,
    pub pending_manual_start: bool,
    pub duration_seconds: f64,
    pub current_path: Option<PathBuf>,
    pub last_output: Option<RecordingOutput>,
    pub error: Option<String>,
}
