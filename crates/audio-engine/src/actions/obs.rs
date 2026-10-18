use std::{
    net::{TcpStream, ToSocketAddrs},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tungstenite::{client, Message};
use uuid::Uuid;

use super::{
    model::{ObsActionConfig, ObsConnectionConfig},
    secrets::SecretStore,
};

pub fn execute(
    action: &ObsActionConfig,
    connection: &ObsConnectionConfig,
    secrets: &SecretStore,
    timeout_ms: u64,
) -> std::result::Result<(), String> {
    action.validate()?;
    connection.validate()?;
    let timeout = Duration::from_millis(timeout_ms);
    let deadline = Instant::now() + timeout;
    let host = connection.host.clone();
    let port = connection.port;
    let (resolved_tx, resolved_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("obs-host-resolver".into())
        .spawn(move || {
            let resolved = (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>());
            let _ = resolved_tx.send(resolved);
        })
        .map_err(|_| "OBS_NOT_CONNECTED: host resolver could not start".to_string())?;
    let socket_address = resolved_rx
        .recv_timeout(remaining(deadline)?)
        .map_err(|_| "OBS_NOT_CONNECTED: host resolution timed out".to_string())?
        .map_err(|_| "OBS_NOT_CONNECTED: host resolution failed".to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| "OBS_NOT_CONNECTED: host resolution returned no address".to_string())?;
    let stream = TcpStream::connect_timeout(&socket_address, remaining(deadline)?)
        .map_err(|_| "OBS_NOT_CONNECTED: connection failed".to_string())?;
    stream
        .set_read_timeout(Some(remaining(deadline)?))
        .map_err(|_| "OBS_NOT_CONNECTED: timeout configuration failed".to_string())?;
    stream
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|_| "OBS_NOT_CONNECTED: timeout configuration failed".to_string())?;
    let url = format!("ws://{}:{}/", connection.host, connection.port);
    let (mut socket, _) = client(url, stream)
        .map_err(|_| "OBS_NOT_CONNECTED: WebSocket handshake failed".to_string())?;

    let hello = read_json(&mut socket, deadline)?;
    if hello.get("op").and_then(Value::as_u64) != Some(0) {
        return Err("OBS_NOT_CONNECTED: invalid Hello message".into());
    }
    let authentication = hello
        .pointer("/d/authentication")
        .map(|auth| {
            let secret_id = connection
                .password_secret_id
                .ok_or_else(|| "OBS_NOT_CONNECTED: password secret is missing".to_string())?;
            let password = secrets
                .get_persistent(secret_id)
                .map_err(|_| {
                    "OBS_NOT_CONNECTED: password secret storage is unavailable".to_string()
                })?
                .ok_or_else(|| "OBS_NOT_CONNECTED: password secret is unavailable".to_string())?;
            let salt = auth
                .get("salt")
                .and_then(Value::as_str)
                .ok_or_else(|| "OBS_NOT_CONNECTED: authentication salt is missing".to_string())?;
            let challenge = auth
                .get("challenge")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "OBS_NOT_CONNECTED: authentication challenge is missing".to_string()
                })?;
            Ok::<_, String>(obs_authentication(&password, salt, challenge))
        })
        .transpose()?;
    let mut identify = json!({ "op": 1, "d": { "rpcVersion": 1, "eventSubscriptions": 0 } });
    if let Some(authentication) = authentication {
        identify["d"]["authentication"] = Value::String(authentication);
    }
    write_json(&mut socket, &identify, deadline)?;
    let identified = read_json(&mut socket, deadline)?;
    if identified.get("op").and_then(Value::as_u64) != Some(2) {
        return Err("OBS_NOT_CONNECTED: identification failed".into());
    }

    let request_id = Uuid::new_v4().to_string();
    write_json(
        &mut socket,
        &json!({
            "op": 6,
            "d": {
                "requestType": "SetCurrentProgramScene",
                "requestId": request_id,
                "requestData": { "sceneName": action.scene_name }
            }
        }),
        deadline,
    )?;
    loop {
        let response = read_json(&mut socket, deadline)?;
        if response.get("op").and_then(Value::as_u64) != Some(7)
            || response.pointer("/d/requestId").and_then(Value::as_str) != Some(&request_id)
        {
            continue;
        }
        if response
            .pointer("/d/requestStatus/result")
            .and_then(Value::as_bool)
            != Some(true)
        {
            return Err("OBS_ACTION_FAILED: scene change was rejected".into());
        }
        break;
    }
    let _ = socket.close(None);
    Ok(())
}

fn remaining(deadline: Instant) -> std::result::Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "OBS_ACTION_FAILED: action timed out".to_string())
}

fn read_json(
    socket: &mut tungstenite::WebSocket<TcpStream>,
    deadline: Instant,
) -> std::result::Result<Value, String> {
    socket
        .get_mut()
        .set_read_timeout(Some(remaining(deadline)?))
        .map_err(|_| "OBS_ACTION_FAILED: timeout configuration failed".to_string())?;
    let message = socket
        .read()
        .map_err(|_| "OBS_ACTION_FAILED: WebSocket read failed".to_string())?;
    let text = message
        .to_text()
        .map_err(|_| "OBS_ACTION_FAILED: invalid WebSocket message".to_string())?;
    serde_json::from_str(text).map_err(|_| "OBS_ACTION_FAILED: invalid JSON response".to_string())
}

fn write_json(
    socket: &mut tungstenite::WebSocket<TcpStream>,
    value: &Value,
    deadline: Instant,
) -> std::result::Result<(), String> {
    socket
        .get_mut()
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|_| "OBS_ACTION_FAILED: timeout configuration failed".to_string())?;
    socket
        .send(Message::Text(value.to_string().into()))
        .map_err(|_| "OBS_ACTION_FAILED: WebSocket write failed".to_string())
}

fn obs_authentication(password: &str, salt: &str, challenge: &str) -> String {
    let secret = BASE64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    BASE64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}
