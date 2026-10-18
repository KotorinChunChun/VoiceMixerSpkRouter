use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeviceApp {
    pub pid: u32,
    pub name: String,
    pub active: bool,
}

/// 専用スレッド上でCOMを初期化し、エンドポイントのセッションを列挙する。
pub fn list_device_apps(endpoint_id: String) -> Result<Vec<DeviceApp>, String> {
    std::thread::spawn(move || enumerate(&endpoint_id))
        .join()
        .map_err(|_| "使用アプリ取得スレッドが終了しました".to_string())?
}

#[cfg(windows)]
fn enumerate(endpoint_id: &str) -> Result<Vec<DeviceApp>, String> {
    use wasapi::{DeviceEnumerator, SessionState};
    wasapi::initialize_mta().ok().map_err(|e| e.to_string())?;
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            wasapi::deinitialize();
        }
    }
    let _guard = ComGuard;
    let read = || -> Result<Vec<DeviceApp>, wasapi::WasapiError> {
        let enumerator = DeviceEnumerator::new()?;
        let device = enumerator.get_device(endpoint_id)?;
        let manager = device.get_iaudiosessionmanager()?;
        let sessions = manager.get_audiosessionenumerator()?;
        let system = sysinfo::System::new_all();
        let mut apps: std::collections::BTreeMap<u32, DeviceApp> = Default::default();
        for index in 0..sessions.get_count()? {
            let session = sessions.get_session(index)?;
            let state = session.get_state()?;
            if matches!(state, SessionState::Expired) {
                continue;
            }
            let pid = session.get_process_id()?;
            let active = matches!(state, SessionState::Active);
            let name = if pid == 0 {
                "システムサウンド".into()
            } else {
                system
                    .process(sysinfo::Pid::from_u32(pid))
                    .map(|p| p.name().to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("アプリ名取得不可 (PID {pid})"))
            };
            apps.entry(pid)
                .and_modify(|app| app.active |= active)
                .or_insert(DeviceApp { pid, name, active });
        }
        Ok(apps.into_values().collect())
    };
    read().map_err(|e| format!("使用アプリを取得できません: {e}"))
}

#[cfg(not(windows))]
fn enumerate(_: &str) -> Result<Vec<DeviceApp>, String> {
    Err("使用アプリの取得はWindowsでのみ利用できます".into())
}
