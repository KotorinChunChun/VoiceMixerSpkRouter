//! OSのキーを予約せず、設定保存と監視割当の確定を分ける。
use super::super::hotkeys::ShortcutObserver;
use super::{Command, SHORTCUT};
use crate::key_binding::{validate_hotkeys, Hotkey, HOTKEY_COUNT};
use windows::Win32::Foundation::HWND;
pub(crate) struct PreparedHotkeys([Hotkey; HOTKEY_COUNT]);
pub(super) struct KeyRegistration {
    observer: ShortcutObserver,
}
impl KeyRegistration {
    pub(super) fn new(hwnd: HWND, keys: &[Hotkey; HOTKEY_COUNT]) -> Result<(Self, String), String> {
        let current = Self {
            observer: ShortcutObserver::new(hwnd, SHORTCUT)?,
        };
        current.commit(current.prepare(keys)?)?;
        Ok((current, String::new()))
    }
    pub(super) fn prepare(&self, keys: &[Hotkey; HOTKEY_COUNT]) -> Result<PreparedHotkeys, String> {
        validate_hotkeys(keys)?;
        Ok(PreparedHotkeys(keys.clone()))
    }
    pub(super) fn commit(&self, keys: PreparedHotkeys) -> Result<(), String> {
        let commands = [
            Command::Device,
            Command::Application,
            Command::Browser,
            Command::Stopped,
            Command::Show,
            Command::ToggleOverlay,
        ];
        self.observer.set_bindings(
            &keys
                .0
                .into_iter()
                .zip(commands)
                .map(|(key, command)| (command as usize, key))
                .collect::<Vec<_>>(),
        )
    }
}
