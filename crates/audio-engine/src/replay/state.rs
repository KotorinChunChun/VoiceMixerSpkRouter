use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPlaybackState {
    Stopped,
    Playing,
    Paused,
}

impl ReplayPlaybackState {
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Playing => 1,
            Self::Paused => 2,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Playing,
            2 => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ReplayStateSnapshot {
    pub state: ReplayPlaybackState,
    pub position_seconds: f64,
    pub duration_seconds: Option<f64>,
    pub eof: bool,
    pub current_file: Option<String>,
}
