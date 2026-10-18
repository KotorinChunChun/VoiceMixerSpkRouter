use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use crate::{
    edge::{control::EdgeControlMessage, receiver::EdgeHub},
    error::{BackendError, Result},
};

pub async fn run_pcm_server(addr: SocketAddr, hub: EdgeHub) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    run_pcm_listener(listener, hub).await
}

/// 事前に確保した待受を使用し、起動時にポート競合を検出する。
pub async fn run_pcm_listener(listener: TcpListener, hub: EdgeHub) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let hub = hub.clone();
        tokio::spawn(async move {
            let _ = handle_pcm_connection(stream, hub).await;
        });
    }
}

async fn handle_pcm_connection(stream: TcpStream, hub: EdgeHub) -> Result<()> {
    let mut ws = accept_async(stream)
        .await
        .map_err(|e| BackendError::Audio(e.to_string()))?;
    let first = ws
        .next()
        .await
        .ok_or(BackendError::ChannelDisconnected)?
        .map_err(|e| BackendError::Audio(e.to_string()))?;
    let hello: EdgeControlMessage = serde_json::from_str(
        first
            .to_text()
            .map_err(|e| BackendError::Audio(e.to_string()))?,
    )?;
    let key = match hello {
        EdgeControlMessage::Hello {
            stream_key,
            sample_rate,
            channels,
            protocol_version,
            token,
            tab_id,
            title,
            url,
        } => {
            if let Err(reason) = hub.authenticate(token.as_deref(), protocol_version) {
                let _ = ws
                    .send(Message::Text(
                        serde_json::to_string(&EdgeControlMessage::Rejected {
                            reason: reason.to_string(),
                        })
                        .unwrap_or_default()
                        .into(),
                    ))
                    .await;
                return Ok(());
            }
            if stream_key.trim().is_empty()
                || !(1..=2).contains(&channels)
                || !(8_000..=192_000).contains(&sample_rate)
            {
                let _ = ws
                    .send(Message::Text(
                        serde_json::to_string(&EdgeControlMessage::Rejected {
                            reason: "format".into(),
                        })
                        .unwrap_or_default()
                        .into(),
                    ))
                    .await;
                return Ok(());
            }
            let lease = match hub.acquire_pcm_lease(stream_key.clone(), sample_rate, channels) {
                Ok(lease) => lease,
                Err(reason) => {
                    let _ = ws
                        .send(Message::Text(
                            serde_json::to_string(&EdgeControlMessage::Rejected {
                                reason: reason.into(),
                            })
                            .unwrap_or_default()
                            .into(),
                        ))
                        .await;
                    return Ok(());
                }
            };
            if let Some(stream) = hub.get(&stream_key) {
                stream.update_tab(tab_id, title.as_deref(), url.as_deref());
            }
            let _ = ws
                .send(Message::Text(
                    serde_json::to_string(&EdgeControlMessage::Accepted)
                        .unwrap_or_default()
                        .into(),
                ))
                .await;
            let _ = ws
                .send(Message::Text(
                    serde_json::to_string(&EdgeControlMessage::ReceiverState {
                        stream_key: stream_key.clone(),
                        state: hub.receiver_state(&stream_key),
                    })
                    .unwrap_or_default()
                    .into(),
                ))
                .await;
            (stream_key, lease)
        }
        _ => {
            return Err(BackendError::InvalidOperation(
                "first PCM websocket message must be hello".into(),
            ))
        }
    };
    let (key, lease) = key;
    let (media_sender, mut media_receiver) = tokio::sync::mpsc::unbounded_channel();
    hub.attach_media_connection(&key, lease.owner(), media_sender);
    loop {
        tokio::select! {
        Some(command) = media_receiver.recv() => {
            let deliver = match &command {
                EdgeControlMessage::MediaCommand { request_id, .. } => {
                    hub.should_send_media_command(&key, lease.owner(), request_id)
                }
                _ => false,
            };
            if !deliver {
                continue;
            }
            ws.send(Message::Text(
                serde_json::to_string(&command).unwrap_or_default().into(),
            )).await.map_err(|e| BackendError::Audio(e.to_string()))?;
        }
        message = ws.next() => {
        let Some(msg) = message else { break; };
        match msg.map_err(|e| BackendError::Audio(e.to_string()))? {
            Message::Binary(data) => {
                let _ = hub.push_pcm_f32le(&key, data.as_ref())?;
            }
            Message::Text(text) => {
                if let Ok(control) = serde_json::from_str::<EdgeControlMessage>(&text) {
                    match control {
                        EdgeControlMessage::Heartbeat { stream_key } if stream_key == key => {
                            let _ = hub.heartbeat(&key);
                            let _ = ws
                                .send(Message::Text(
                                    serde_json::to_string(&EdgeControlMessage::ReceiverState {
                                        stream_key: key.clone(),
                                        state: hub.receiver_state(&key),
                                    })
                                    .unwrap_or_default()
                                    .into(),
                                ))
                                .await;
                        }
                        EdgeControlMessage::CaptureState { stream_key, state }
                            if stream_key == key =>
                        {
                            hub.set_capture_state(&key, state)?;
                            let _ = ws
                                .send(Message::Text(
                                    serde_json::to_string(
                                        &EdgeControlMessage::CaptureStateChanged {
                                            stream_key: key.clone(),
                                            state,
                                        },
                                    )
                                    .unwrap_or_default()
                                    .into(),
                                ))
                                .await;
                        }
                        EdgeControlMessage::MediaResult { stream_key, request_id, state, error }
                            if stream_key == key =>
                        {
                            hub.resolve_media_result(&key, lease.owner(), crate::edge::receiver::EdgeMediaResult {
                                stream_key,
                                request_id,
                                state,
                                error,
                            });
                        }
                        EdgeControlMessage::MediaState { stream_key, state, error }
                            if stream_key == key =>
                        {
                            hub.update_media_state(&key, state, error);
                        }
                        EdgeControlMessage::TabUpdated {
                            stream_key,
                            title,
                            url,
                        } if stream_key == key => {
                            if let Some(stream) = hub.get(&key) {
                                stream.update_tab(None, Some(&title), Some(&url));
                            }
                        }
                        EdgeControlMessage::Closing { stream_key } if stream_key == key => break,
                        _ => {}
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {
                let _ = hub.heartbeat(&key);
            }
            _ => {}
        }
        }
        }
    }
    Ok(())
}

pub async fn run_control_server(addr: SocketAddr, hub: EdgeHub) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            let _ = handle_control_connection(stream, hub).await;
        });
    }
}

async fn handle_control_connection(stream: TcpStream, hub: EdgeHub) -> Result<()> {
    let mut ws = accept_async(stream)
        .await
        .map_err(|e| BackendError::Audio(e.to_string()))?;
    let mut authorized_key: Option<String> = None;
    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| BackendError::Audio(e.to_string()))?;
        if !msg.is_text() {
            continue;
        }
        let control: EdgeControlMessage = serde_json::from_str(
            msg.to_text()
                .map_err(|e| BackendError::Audio(e.to_string()))?,
        )?;
        match control {
            EdgeControlMessage::Hello {
                stream_key,
                sample_rate,
                channels,
                protocol_version,
                token,
                tab_id,
                title,
                url,
            } => {
                if hub
                    .authenticate(token.as_deref(), protocol_version)
                    .is_err()
                {
                    continue;
                }
                if stream_key.trim().is_empty()
                    || !(1..=2).contains(&channels)
                    || !(8_000..=192_000).contains(&sample_rate)
                {
                    continue;
                }
                authorized_key = Some(stream_key.clone());
                if let Some(stream) = hub.get(&stream_key) {
                    stream.update_tab(tab_id, title.as_deref(), url.as_deref());
                }
            }
            EdgeControlMessage::Heartbeat { stream_key }
                if authorized_key.as_deref() == Some(&stream_key) =>
            {
                let _ = hub.heartbeat(&stream_key);
            }
            EdgeControlMessage::TabClosed { .. }
            | EdgeControlMessage::ExtensionStopped { .. }
            | EdgeControlMessage::Closing { .. } => {}
            EdgeControlMessage::TabUpdated {
                stream_key,
                title,
                url,
            } if authorized_key.as_deref() == Some(&stream_key) => {
                if let Some(stream) = hub.get(&stream_key) {
                    stream.update_tab(None, Some(&title), Some(&url));
                }
            }
            EdgeControlMessage::Accepted
            | EdgeControlMessage::Rejected { .. }
            | EdgeControlMessage::CaptureState { .. }
            | EdgeControlMessage::CaptureStateChanged { .. }
            | EdgeControlMessage::ReceiverState { .. }
            | EdgeControlMessage::MediaCommand { .. }
            | EdgeControlMessage::MediaResult { .. }
            | EdgeControlMessage::MediaState { .. } => {}
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::handle_pcm_connection;
    use crate::edge::{
        control::{EdgeCaptureState, EdgeControlMessage},
        receiver::{EdgeHub, EdgeStreamState},
    };
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    async fn connect(
        listener: Arc<TcpListener>,
        hub: EdgeHub,
    ) -> (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio::task::JoinHandle<()>,
    ) {
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = handle_pcm_connection(stream, hub).await;
        });
        let client = connect_async(format!("ws://{address}")).await.unwrap().0;
        (client, task)
    }

    fn hello(hub: &EdgeHub, key: &str) -> Message {
        Message::Text(
            serde_json::to_string(&EdgeControlMessage::hello_v2(
                key,
                hub.auth_token(),
                1,
                "tab",
                "https://example.test",
                48_000,
                2,
            ))
            .unwrap()
            .into(),
        )
    }

    async fn next_control(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> EdgeControlMessage {
        let message = tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(message.into_text().unwrap().as_ref()).unwrap()
    }

    #[tokio::test]
    async fn pcm_connection_rejects_duplicate_key_and_releases_after_invalid_pcm() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut first, _first_task) = connect(listener.clone(), hub.clone()).await;
        first.send(hello(&hub, "same")).await.unwrap();
        assert!(
            matches!(first.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("accepted"))
        );
        let (mut duplicate, _duplicate_task) = connect(listener.clone(), hub.clone()).await;
        duplicate.send(hello(&hub, "same")).await.unwrap();
        assert!(
            matches!(duplicate.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("stream_in_use"))
        );
        first
            .send(Message::Binary(vec![1, 2, 3].into()))
            .await
            .unwrap();
        while first.next().await.is_some() {}
        let (mut reconnect, _reconnect_task) = connect(listener.clone(), hub.clone()).await;
        reconnect.send(hello(&hub, "same")).await.unwrap();
        assert!(
            matches!(reconnect.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("accepted"))
        );
    }

    #[tokio::test]
    async fn different_keys_and_ports_are_independent_and_abort_releases_lease() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut first, first_task) = connect(listener.clone(), hub.clone()).await;
        first.send(hello(&hub, "tab-a")).await.unwrap();
        assert!(
            matches!(first.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("accepted"))
        );
        let (mut other, _other_task) = connect(listener.clone(), hub.clone()).await;
        other.send(hello(&hub, "tab-b")).await.unwrap();
        assert!(
            matches!(other.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("accepted"))
        );

        first_task.abort();
        let _ = first_task.await;
        let (mut after_abort, _after_abort_task) = connect(listener.clone(), hub.clone()).await;
        after_abort.send(hello(&hub, "tab-a")).await.unwrap();
        assert!(
            matches!(after_abort.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("accepted"))
        );

        let other_listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let other_hub = EdgeHub::default();
        let (mut other_port, _other_port_task) = connect(other_listener, other_hub.clone()).await;
        other_port.send(hello(&other_hub, "tab-a")).await.unwrap();
        assert!(
            matches!(other_port.next().await.unwrap().unwrap(), Message::Text(text) if text.contains("accepted"))
        );
    }

    #[tokio::test]
    async fn unauthenticated_control_cannot_change_pcm_owner_state_or_metadata() {
        let pcm_listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let control_listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut pcm, _pcm_task) = connect(pcm_listener, hub.clone()).await;
        pcm.send(hello(&hub, "owned")).await.unwrap();
        let _ = pcm.next().await.unwrap().unwrap();
        let address = control_listener.local_addr().unwrap();
        let control_hub = hub.clone();
        let control_task = tokio::spawn(async move {
            let (stream, _) = control_listener.accept().await.unwrap();
            let _ = super::handle_control_connection(stream, control_hub).await;
        });
        let (mut control, _) = connect_async(format!("ws://{address}")).await.unwrap();
        for message in [
            EdgeControlMessage::Closing {
                stream_key: "owned".into(),
            },
            EdgeControlMessage::Heartbeat {
                stream_key: "owned".into(),
            },
            EdgeControlMessage::TabUpdated {
                stream_key: "owned".into(),
                title: "changed".into(),
                url: "https://changed.test".into(),
            },
        ] {
            control
                .send(Message::Text(
                    serde_json::to_string(&message).unwrap().into(),
                ))
                .await
                .unwrap();
        }
        control.close(None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), control_task)
            .await
            .unwrap()
            .unwrap();
        let stream = hub.get("owned").unwrap();
        assert_eq!(stream.title(), "tab");
        assert_eq!(stream.url(), "https://example.test");
        assert_eq!(
            stream.state(std::time::Duration::from_secs(1)),
            crate::edge::receiver::EdgeStreamState::Running
        );
    }

    #[tokio::test]
    async fn capture_stop_acknowledges_and_heartbeat_does_not_resume_it() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut socket, _task) = connect(listener, hub.clone()).await;
        socket.send(hello(&hub, "capture")).await.unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::Accepted
        ));
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::ReceiverState {
                state: EdgeCaptureState::Stopped,
                ..
            }
        ));

        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::CaptureState {
                    stream_key: "capture".into(),
                    state: EdgeCaptureState::Stopped,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::CaptureStateChanged {
                state: EdgeCaptureState::Stopped,
                ..
            }
        ));
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::Heartbeat {
                    stream_key: "capture".into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::ReceiverState {
                state: EdgeCaptureState::Stopped,
                ..
            }
        ));
        assert_eq!(
            hub.get("capture")
                .unwrap()
                .state(std::time::Duration::from_secs(1)),
            EdgeStreamState::Stopped
        );

        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::CaptureState {
                    stream_key: "capture".into(),
                    state: EdgeCaptureState::Running,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::CaptureStateChanged {
                state: EdgeCaptureState::Running,
                ..
            }
        ));
        assert_eq!(
            hub.get("capture")
                .unwrap()
                .state(std::time::Duration::from_secs(1)),
            EdgeStreamState::Running
        );
    }

    #[tokio::test]
    async fn heartbeat_reports_receiver_state_after_graph_consumer_toggles() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let consumer = crate::audio::graph::NodeId::new();
        hub.set_receiver_enabled("receiver", consumer, true);
        let (mut socket, _task) = connect(listener, hub.clone()).await;
        socket.send(hello(&hub, "receiver")).await.unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::Accepted
        ));
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::ReceiverState {
                state: EdgeCaptureState::Running,
                ..
            }
        ));
        hub.set_receiver_enabled("receiver", consumer, false);
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::Heartbeat {
                    stream_key: "receiver".into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::ReceiverState {
                state: EdgeCaptureState::Stopped,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn media_command_uses_pcm_owner_connection_and_waits_for_matching_result() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut socket, _task) = connect(listener, hub.clone()).await;
        socket.send(hello(&hub, "media")).await.unwrap();
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::Accepted
        ));
        let _ = next_control(&mut socket).await;

        let request_hub = hub.clone();
        let request = tokio::spawn(async move {
            request_hub
                .control_media("media", crate::edge::control::EdgeMediaAction::Pause)
                .await
        });
        let command = next_control(&mut socket).await;
        let EdgeControlMessage::MediaCommand {
            stream_key,
            request_id,
            action,
        } = command
        else {
            panic!("media_command was not sent to the PCM owner");
        };
        assert_eq!(stream_key, "media");
        assert_eq!(action, crate::edge::control::EdgeMediaAction::Pause);
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::MediaResult {
                    stream_key: stream_key.clone(),
                    request_id: "stale-request".into(),
                    state: crate::edge::control::EdgeMediaState::Playing,
                    error: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(
            !request.is_finished(),
            "mismatched request_id must be ignored"
        );
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::MediaResult {
                    stream_key,
                    request_id,
                    state: crate::edge::control::EdgeMediaState::Paused,
                    error: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let result = request.await.unwrap().unwrap();
        assert_eq!(result.state, crate::edge::control::EdgeMediaState::Paused);
        assert_eq!(
            hub.list_tabs()[0].media_state,
            crate::edge::control::EdgeMediaState::Paused
        );
    }

    #[tokio::test]
    async fn media_pending_request_fails_when_pcm_owner_disconnects() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut socket, _task) = connect(listener, hub.clone()).await;
        socket.send(hello(&hub, "disconnect-media")).await.unwrap();
        let _ = next_control(&mut socket).await;
        let _ = next_control(&mut socket).await;

        let request_hub = hub.clone();
        let request = tokio::spawn(async move {
            request_hub
                .control_media(
                    "disconnect-media",
                    crate::edge::control::EdgeMediaAction::Play,
                )
                .await
        });
        assert!(matches!(
            next_control(&mut socket).await,
            EdgeControlMessage::MediaCommand { .. }
        ));
        socket.close(None).await.unwrap();
        let error = request.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("切断"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn media_result_error_or_opposite_state_is_not_reported_as_success() {
        let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
        let hub = EdgeHub::default();
        let (mut socket, _task) = connect(listener, hub.clone()).await;
        socket.send(hello(&hub, "media-error")).await.unwrap();
        let _ = next_control(&mut socket).await;
        let _ = next_control(&mut socket).await;

        let first_hub = hub.clone();
        let first = tokio::spawn(async move {
            first_hub
                .control_media("media-error", crate::edge::control::EdgeMediaAction::Pause)
                .await
        });
        let EdgeControlMessage::MediaCommand {
            stream_key,
            request_id,
            ..
        } = next_control(&mut socket).await
        else {
            panic!("expected media command")
        };
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::MediaResult {
                    stream_key,
                    request_id,
                    state: crate::edge::control::EdgeMediaState::Paused,
                    error: Some("ページが制御を拒否しました".into()),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(first
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("拒否"));

        let second_hub = hub.clone();
        let second = tokio::spawn(async move {
            second_hub
                .control_media("media-error", crate::edge::control::EdgeMediaAction::Play)
                .await
        });
        let EdgeControlMessage::MediaCommand {
            stream_key,
            request_id,
            ..
        } = next_control(&mut socket).await
        else {
            panic!("expected media command")
        };
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeControlMessage::MediaResult {
                    stream_key,
                    request_id,
                    state: crate::edge::control::EdgeMediaState::Paused,
                    error: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(second.await.unwrap().is_err());
    }
}
