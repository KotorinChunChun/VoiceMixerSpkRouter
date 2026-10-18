use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EdgeControlMessage {
    Hello {
        stream_key: String,
        sample_rate: u32,
        channels: u16,
        #[serde(default)]
        protocol_version: u16,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        tab_id: Option<u64>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    Accepted,
    Rejected {
        reason: String,
    },
    Heartbeat {
        stream_key: String,
    },
    /// Capture is intentionally paused while the PCM socket remains alive.
    CaptureState {
        stream_key: String,
        state: EdgeCaptureState,
    },
    CaptureStateChanged {
        stream_key: String,
        state: EdgeCaptureState,
    },
    /// Tells the extension whether at least one graph input consumes this key.
    ReceiverState {
        stream_key: String,
        state: EdgeCaptureState,
    },
    MediaCommand {
        stream_key: String,
        request_id: String,
        action: EdgeMediaAction,
    },
    MediaResult {
        stream_key: String,
        request_id: String,
        state: EdgeMediaState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    MediaState {
        stream_key: String,
        state: EdgeMediaState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    TabUpdated {
        stream_key: String,
        title: String,
        url: String,
    },
    Closing {
        stream_key: String,
    },
    TabClosed {
        stream_key: String,
    },
    ExtensionStopped {
        stream_key: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCaptureState {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeMediaAction {
    Play,
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeMediaState {
    Playing,
    Paused,
    Unsupported,
    Unknown,
}

impl EdgeControlMessage {
    pub fn hello_v2(
        stream_key: impl Into<String>,
        token: impl Into<String>,
        tab_id: u64,
        title: impl Into<String>,
        url: impl Into<String>,
        sample_rate: u32,
        channels: u16,
    ) -> Self {
        Self::Hello {
            stream_key: stream_key.into(),
            sample_rate,
            channels,
            protocol_version: 1,
            token: Some(token.into()),
            tab_id: Some(tab_id),
            title: Some(title.into()),
            url: Some(url.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EdgeControlMessage;

    #[test]
    fn hello_v2_serializes_protocol_metadata() {
        let message = EdgeControlMessage::hello_v2(
            "stream",
            "token",
            7,
            "Example",
            "https://example.test",
            48_000,
            2,
        );
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value["type"], "hello");
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["tab_id"], 7);
        assert_eq!(value["channels"], 2);
    }
}
