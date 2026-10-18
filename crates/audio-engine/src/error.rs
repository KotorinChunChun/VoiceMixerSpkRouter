use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("graph validation failed: {0}")]
    GraphValidation(String),
    #[error("node not found: {0}")]
    NodeNotFound(String),
    #[error("edge not found: {0}")]
    EdgeNotFound(String),
    #[error("port not found: {0}")]
    PortNotFound(String),
    #[error("invalid operation: {0}")]
    InvalidOperation(String),
    #[error("preset not found: {0}")]
    PresetNotFound(String),
    #[error("preset already exists: {0}")]
    PresetAlreadyExists(String),
    #[error("audio backend error: {0}")]
    Audio(String),
    #[error("transform plugin error: {0}")]
    Plugin(String),
    #[error("legacy RVC backend error: {0}")]
    Rvc(String),
    #[error("platform feature is unavailable: {0}")]
    PlatformUnsupported(String),
    #[error("configuration file error at {path:?}: {source}")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("channel disconnected")]
    ChannelDisconnected,
}

pub type Result<T> = std::result::Result<T, BackendError>;
