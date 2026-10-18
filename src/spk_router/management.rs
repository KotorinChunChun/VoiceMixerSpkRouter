//! KotorinToys管理契約2。ユーザー専用パイプでUI巡回と停止受理を分ける。
use crate::{config::APP_VERSION, ui::Ui};
use serde::{Deserialize, Serialize};
use std::os::windows::io::AsRawHandle;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::windows::named_pipe::ServerOptions,
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::{
            Pipes::GetNamedPipeClientProcessId,
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    },
};
pub const TOOL_ID: &str = "voice-mixer-spk-router";
const LIMIT: usize = 8192;
const DEADLINE: Duration = Duration::from_secs(2);
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    command: String,
    expected_pid: Option<u32>,
    #[serde(default)]
    request_id: String,
}
impl Request {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let line = std::str::from_utf8(bytes)
            .map_err(|_| "UTF-8ではありません")?
            .trim();
        if line == "status" {
            return Ok(Self {
                command: line.into(),
                expected_pid: None,
                request_id: String::new(),
            });
        }
        serde_json::from_str(line).map_err(|_| "管理要求の形式が不正です".into())
    }
    fn refusal(&self, pid: u32) -> Option<&'static str> {
        if self.expected_pid.is_some_and(|expected| expected != pid) {
            Some("対象PIDが変わりました。")
        } else if matches!(self.command.as_str(), "show" | "stop") && self.expected_pid.is_none() {
            Some("操作対象のPIDが必要です。")
        } else {
            None
        }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    protocol_version: u8,
    request_id: String,
    pid: u32,
    ready: bool,
    version: &'static str,
    launch_origin: String,
    capabilities: [&'static str; 3],
    result: String,
    detail: String,
}
impl Reply {
    fn new(request_id: String, origin: &str, result: &str, detail: &str) -> Self {
        Self {
            protocol_version: 2,
            request_id,
            pid: std::process::id(),
            ready: true,
            version: APP_VERSION,
            launch_origin: origin.into(),
            capabilities: ["status", "show", "stop"],
            result: result.into(),
            detail: detail.into(),
        }
    }
}
enum Event {
    Request {
        request: Request,
        expires: Instant,
        reply: mpsc::SyncSender<Reply>,
    },
    Stop {
        expires: Instant,
    },
}
pub struct Endpoint {
    events: mpsc::Receiver<Event>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    origin: String,
}
impl Endpoint {
    pub fn start(origin: String) -> Result<Self, String> {
        let (send, events) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let shutdown = stopped.clone();
        let worker_origin = origin.clone();
        let worker = thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(value) => value,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                let result = security_and_pipe(true);
                let mut pipe = match result {
                    Ok(pipe) => {
                        let _ = ready.send(Ok(()));
                        pipe
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while !shutdown.load(Ordering::Acquire) {
                    tokio::select! {
                        _=tokio::time::sleep(Duration::from_millis(100))=>continue,
                        connected=pipe.connect()=>if connected.is_err(){let _=pipe.disconnect();continue;}
                    }
                    let next = match security_and_pipe(false) { Ok(next)=>next, Err(_)=>break };
                    let mut connection = std::mem::replace(&mut pipe, next);
                    let exchange = async {
                        // ACLに加え、対話セッションの違うクライアントを拒否する。
                        let mut client = 0;
                        let mut theirs = 0;
                        let mut ours = 0;
                        unsafe {
                            GetNamedPipeClientProcessId(HANDLE(connection.as_raw_handle()), &mut client)
                                .map_err(|_| ())?;
                            ProcessIdToSessionId(client, &mut theirs).map_err(|_| ())?;
                            ProcessIdToSessionId(std::process::id(), &mut ours).map_err(|_| ())?;
                        }
                        if theirs != ours {
                            return Err(());
                        }
                        let expires = Instant::now() + DEADLINE;
                        let mut bytes = Vec::new();
                        while bytes.len() < LIMIT {
                            let byte = connection.read_u8().await.map_err(|_| ())?;
                            if byte == b'\n' {
                                break;
                            }
                            bytes.push(byte);
                        }
                        if bytes.len() == LIMIT {
                            return Err(());
                        }
                        let request = match Request::parse(&bytes) {
                            Ok(request) => request,
                            Err(detail) => {
                                let reply =
                                    Reply::new(String::new(), &worker_origin, "failed", &detail);
                                let mut data = serde_json::to_vec(&reply).map_err(|_| ())?;
                                data.push(b'\n');
                                connection.write_all(&data).await.map_err(|_| ())?;
                                return Ok(());
                            }
                        };
                        let (reply, response) = mpsc::sync_channel(1);
                        send.send(Event::Request {
                            request,
                            expires,
                            reply,
                        })
                        .map_err(|_| ())?;
                        // 専用スレッドだけを待たせる。UIを止めず、期限超過要求は巡回で破棄する。
                        let reply = response
                            .recv_timeout(expires.saturating_duration_since(Instant::now()))
                            .map_err(|_| ())?;
                        let stop = reply.result == "accepted";
                        let mut data = serde_json::to_vec(&reply).map_err(|_| ())?;
                        data.push(b'\n');
                        connection.write_all(&data).await.map_err(|_| ())?;
                        if stop {
                            send.send(Event::Stop { expires }).map_err(|_| ())?;
                        }
                        Ok::<(), ()>(())
                    };
                    let _ = tokio::time::timeout(DEADLINE, exchange).await;
                    let _ = connection.disconnect();
                }
            });
        });
        match started.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                events,
                stopped,
                worker: Some(worker),
                origin,
            }),
            result => {
                stopped.store(true, Ordering::Release);
                let _ = worker.join();
                Err(format!("管理窓口を開始できません: {result:?}"))
            }
        }
    }
    pub fn poll(&self, ui: &mut Ui) -> bool {
        for event in self.events.try_iter() {
            match event {
                Event::Stop { expires }
                    if Instant::now() < expires && ui.management_stop_block().is_none() =>
                {
                    return true
                }
                Event::Stop { .. } => {}
                Event::Request {
                    request,
                    expires,
                    reply,
                } => {
                    if Instant::now() >= expires {
                        continue;
                    }
                    let (result, detail) = if let Some(reason) = request.refusal(std::process::id())
                    {
                        ("refused", reason)
                    } else {
                        match request.command.as_str() {
                            "status" => ("completed", ""),
                            "show" => {
                                ui.show_settings();
                                if ui.settings_is_open() {
                                    ("completed", "")
                                } else {
                                    ("failed", "設定画面を表示できません。")
                                }
                            }
                            "stop" => {
                                if let Some(reason) = ui.management_stop_block() {
                                    ("refused", reason)
                                } else {
                                    ("accepted", "")
                                }
                            }
                            _ => ("unsupported", "未対応の操作です。"),
                        }
                    };
                    let _ =
                        reply.send(Reply::new(request.request_id, &self.origin, result, detail));
                }
            }
        }
        false
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn user_sid() -> Result<String, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| e.to_string())?;
        let result = (|| {
            let mut size = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
            .map_err(|e| e.to_string())?;
            let token_user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
            let mut text = PWSTR::null();
            ConvertSidToStringSidW(token_user.User.Sid, &mut text).map_err(|e| e.to_string())?;
            let value = text.to_string().map_err(|e| e.to_string());
            LocalFree(Some(HLOCAL(text.0.cast())));
            value
        })();
        let _ = CloseHandle(token);
        result
    }
}
fn security_and_pipe(
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, String> {
    let sid = user_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            1,
            &mut descriptor,
            None,
        )
        .map_err(|e| e.to_string())?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let pipe = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                format!(r"\\.\pipe\KotorinToys.{sid}.{TOOL_ID}"),
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
            .map_err(|e| e.to_string());
        LocalFree(Some(HLOCAL(descriptor.0)));
        pipe
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operations_need_exact_pid_and_status_is_legacy_compatible() {
        assert!(Request::parse(b"status").unwrap().refusal(42).is_none());
        assert!(Request::parse(br#"{"command":"stop"}"#)
            .unwrap()
            .refusal(42)
            .is_some());
        assert!(Request::parse(br#"{"command":"stop","expectedPid":41}"#)
            .unwrap()
            .refusal(42)
            .is_some());
        assert!(
            Request::parse(br#"{"command":"show","expectedPid":42,"requestId":"r"}"#)
                .unwrap()
                .refusal(42)
                .is_none()
        );
        assert!(Request::parse(b"invalid").is_err());
    }
    #[test]
    fn reply_matches_host_contract_and_does_not_disclose_settings() {
        let reply =
            serde_json::to_value(Reply::new("r".into(), "host_manual", "accepted", "")).unwrap();
        assert_eq!(reply["protocolVersion"], 2);
        assert_eq!(reply["requestId"], "r");
        assert_eq!(reply["version"], APP_VERSION);
        assert!(reply.get("browser_token").is_none());
        assert!(reply["ready"].as_bool().unwrap());
    }
}
