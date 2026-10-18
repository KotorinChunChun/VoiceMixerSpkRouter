//! セッション全体の排他起動と既存常駐への操作転送。
use super::{
    cli::Launch,
    ui::{self, ExistingCommand},
};
use std::{thread, time::Duration};
use windows::{
    core::w,
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE},
        System::Threading::CreateMutexW,
    },
};

const STARTUP_ATTEMPTS: usize = 40;
const STARTUP_INTERVAL: Duration = Duration::from_millis(50);

pub struct Instance(HANDLE);

pub enum Acquisition {
    Forwarded,
    Owner(Instance),
}

fn command(launch: Launch) -> ExistingCommand {
    match launch {
        Launch::Restore | Launch::Settings => ExistingCommand::ShowSettings,
        Launch::Stop => ExistingCommand::Exit,
        Launch::Mode(mode) => ExistingCommand::SetMode(mode),
    }
}

impl Instance {
    /// Noneは診断専用。既存常駐へ転送せず、通常起動と同じ排他範囲を使う。
    pub fn acquire(launch: Option<Launch>) -> Result<Acquisition, String> {
        if let Some(launch) = launch {
            if ui::send_existing(command(launch))? {
                return Ok(Acquisition::Forwarded);
            }
            if launch == Launch::Stop {
                return Err("VoiceMixerSpkRouterは起動していません".into());
            }
        }
        let (instance, already_exists) = unsafe {
            let handle = CreateMutexW(None, false, w!("Local\\VoiceMixerSpkRouter.v1"))
                .map_err(|e| e.to_string())?;
            (Self(handle), GetLastError() == ERROR_ALREADY_EXISTS)
        };
        if already_exists {
            if let Some(launch) = launch {
                for _ in 0..STARTUP_ATTEMPTS {
                    if ui::send_existing(command(launch))? {
                        return Ok(Acquisition::Forwarded);
                    }
                    thread::sleep(STARTUP_INTERVAL);
                }
            }
            return Err("VoiceMixerSpkRouterは既に起動中です".into());
        }
        Ok(Acquisition::Owner(instance))
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
