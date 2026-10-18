use std::sync::Arc;

use parking_lot::Mutex;

use super::model::{HotkeyActionConfig, HotkeyTarget, KeyCode};

pub trait KeyInputSender: Send + Sync {
    fn key_down(&self, key: KeyCode) -> std::result::Result<(), String>;
    fn key_up(&self, key: KeyCode) -> std::result::Result<(), String>;
}

pub struct HotkeyExecutor {
    sender: Arc<dyn KeyInputSender>,
    serialization: Mutex<()>,
}

impl HotkeyExecutor {
    pub fn platform() -> Self {
        Self::new(Arc::new(PlatformKeyInputSender))
    }

    pub fn new(sender: Arc<dyn KeyInputSender>) -> Self {
        Self {
            sender,
            serialization: Mutex::new(()),
        }
    }

    pub fn execute(&self, config: &HotkeyActionConfig) -> std::result::Result<(), String> {
        config.validate()?;
        if config.target != HotkeyTarget::Global {
            return Err("HOTKEY_SEND_FAILED: unsupported hotkey target".into());
        }
        let _serialization = self.serialization.lock();
        let mut guard = ReleaseGuard {
            sender: self.sender.as_ref(),
            pressed: Vec::with_capacity(config.keys.len()),
        };
        for &key in &config.keys {
            guard
                .sender
                .key_down(key)
                .map_err(|_| "HOTKEY_SEND_FAILED: key press failed".to_string())?;
            guard.pressed.push(key);
        }
        guard.release_all()
    }
}

struct ReleaseGuard<'a> {
    sender: &'a dyn KeyInputSender,
    pressed: Vec<KeyCode>,
}

impl ReleaseGuard<'_> {
    fn release_all(&mut self) -> std::result::Result<(), String> {
        let mut failed = false;
        while let Some(key) = self.pressed.pop() {
            if self.sender.key_up(key).is_err() {
                failed = true;
            }
        }
        if failed {
            Err("HOTKEY_SEND_FAILED: key release failed".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        while let Some(key) = self.pressed.pop() {
            let _ = self.sender.key_up(key);
        }
    }
}

struct PlatformKeyInputSender;

#[cfg(windows)]
impl KeyInputSender for PlatformKeyInputSender {
    fn key_down(&self, key: KeyCode) -> std::result::Result<(), String> {
        send_key(key, false)
    }

    fn key_up(&self, key: KeyCode) -> std::result::Result<(), String> {
        send_key(key, true)
    }
}

#[cfg(windows)]
fn send_key(key: KeyCode, release: bool) -> std::result::Result<(), String> {
    use std::mem::size_of;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };

    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(key.0),
                wScan: 0,
                dwFlags: if release {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err("SendInput failed".into())
    }
}

#[cfg(not(windows))]
impl KeyInputSender for PlatformKeyInputSender {
    fn key_down(&self, _key: KeyCode) -> std::result::Result<(), String> {
        Err("global hotkey actions are only supported on Windows".into())
    }

    fn key_up(&self, _key: KeyCode) -> std::result::Result<(), String> {
        Ok(())
    }
}
