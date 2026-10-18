use crate::config::Settings;
use audio_mixer_backend::{audio::graph::NodeProperties, edge::receiver::EdgeHub};

pub(super) fn reset_browser_buffer(hub: &EdgeHub, properties: Option<&NodeProperties>) {
    if let Some(NodeProperties::EdgeTab { stream_key, .. }) = properties {
        if let Some(stream) = hub.get(stream_key) {
            stream.jitter.reset();
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ConnectionSettings {
    port: u16,
    token: String,
    stream_key: String,
}

impl From<&Settings> for ConnectionSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            port: settings.browser_port,
            token: settings.browser_token.clone(),
            stream_key: settings.browser_stream_key.clone(),
        }
    }
}

/// 待受の寿命は取得モードと独立させ、停止中も拡張の接続を保持する。
#[derive(Default)]
pub(super) struct BrowserReceiver {
    server: Option<BrowserServer>,
    settings: Option<ConnectionSettings>,
    error: Option<String>,
}

impl BrowserReceiver {
    pub(super) fn disable(&mut self) {
        self.stop();
        self.settings = None;
        self.error = None;
    }
    pub(super) fn needs_restart(&self, settings: &Settings) -> bool {
        self.settings.as_ref() != Some(&ConnectionSettings::from(settings))
            || self.server.as_ref().is_none_or(BrowserServer::is_finished)
    }

    pub(super) fn configure(&mut self, settings: &Settings, hub: EdgeHub) -> Result<(), String> {
        // 旧認証済み接続を閉じてからトークンと待受を差し替える。
        self.stop();
        self.settings = None;
        hub.set_auth_token(settings.browser_token.clone());
        match BrowserServer::start(settings.browser_port, hub) {
            Ok(server) => {
                self.server = Some(server);
                self.error = None;
                self.settings = Some(ConnectionSettings::from(settings));
                Ok(())
            }
            Err(error) => {
                self.error = Some(error.clone());
                Err(error)
            }
        }
    }

    pub(super) fn poll(&mut self) {
        if self.server.as_ref().is_some_and(BrowserServer::is_finished) {
            self.stop();
            self.error = Some("ブラウザの待受が終了しました。再接続します".into());
        }
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn stop(&mut self) {
        self.server.take();
    }
}

#[cfg(feature = "edge-websocket")]
struct BrowserServer {
    runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<audio_mixer_backend::Result<()>>,
}

#[cfg(feature = "edge-websocket")]
impl BrowserServer {
    fn start(port: u16, hub: EdgeHub) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|error| format!("ブラウザ待受ポート{port}を使用できません: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let task = {
            let _guard = runtime.enter();
            let listener =
                tokio::net::TcpListener::from_std(listener).map_err(|error| error.to_string())?;
            runtime.spawn(audio_mixer_backend::edge::websocket::run_pcm_listener(
                listener, hub,
            ))
        };
        Ok(Self { runtime, task })
    }

    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

#[cfg(not(feature = "edge-websocket"))]
struct BrowserServer;

#[cfg(not(feature = "edge-websocket"))]
impl BrowserServer {
    fn start(_port: u16, _hub: EdgeHub) -> Result<Self, String> {
        Err("ブラウザ音声にはedge-websocketを有効にしたビルドが必要です".into())
    }

    fn is_finished(&self) -> bool {
        true
    }
}

#[cfg(feature = "edge-websocket")]
impl Drop for BrowserServer {
    fn drop(&mut self) {
        // Runtimeの破棄は待受と接続済みタスクをまとめて停止する。
        self.task.abort();
        let _ = &self.runtime;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_reselection_discards_pcm_accumulated_while_disabled() {
        let hub = EdgeHub::default();
        let stream = hub.ensure("selected", 48_000, 2);
        stream.jitter.push(&vec![0.5; 48_000]);
        let properties = NodeProperties::EdgeTab {
            stream_key: "selected".into(),
            gain_db: 0.0,
            mute: false,
        };
        reset_browser_buffer(&hub, Some(&properties));
        let mut samples = [1.0; 480];
        assert_eq!(stream.jitter.read_into(&mut samples), 0);
        assert!(samples.iter().all(|sample| *sample == 0.0));
    }
}
