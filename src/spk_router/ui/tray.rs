//! 通知窓、トレイ、アイコンと通知先に結び付くホットキーを所有する。
use super::{
    commands::{queue, Command, ExistingCommand},
    native::{title, wide, WindowClass},
};
use crate::{
    config::{Mode, Settings},
    key_binding::{Hotkey, HOTKEY_COUNT},
};
use std::sync::OnceLock;
#[path = "key_registration.rs"]
mod key_registration;
use key_registration::KeyRegistration;
pub(crate) use key_registration::PreparedHotkeys;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        Graphics::Gdi::HBRUSH,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
};
const CLASS: &str = "VoiceMixerSpkRouterV1";
const CALLBACK: u32 = WM_APP + 81;
pub(super) const CONTROL: u32 = WM_APP + 82;
pub(super) const SHORTCUT: u32 = WM_APP + 83;
static TASKBAR: OnceLock<u32> = OnceLock::new();
fn taskbar() -> u32 {
    *TASKBAR
        .get_or_init(|| unsafe { RegisterWindowMessageW(PCWSTR(wide("TaskbarCreated").as_ptr())) })
}
pub(super) fn send_existing(command: ExistingCommand) -> Result<bool, String> {
    unsafe {
        match FindWindowW(PCWSTR(wide(CLASS).as_ptr()), PCWSTR::null()) {
            Ok(hwnd) => {
                PostMessageW(
                    Some(hwnd),
                    CONTROL,
                    WPARAM(command.command() as usize),
                    LPARAM(0),
                )
                .map_err(|error| error.to_string())?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
}
unsafe extern "system" fn resident_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == SHORTCUT {
        if super::hotkeys::accepts_notification(w.0, l.0 as usize) {
            if let Some(command) = Command::from_id(w.0) {
                super::commands::queue_shortcut(command);
            }
        }
        return LRESULT(0);
    }
    let command = match msg {
        CONTROL | WM_COMMAND => Command::from_id(w.0 & 0xffff),
        WM_CLOSE => Some(Command::Exit),
        CALLBACK if matches!(l.0 as u32, WM_RBUTTONUP | WM_CONTEXTMENU) => Some(Command::Menu),
        CALLBACK if l.0 as u32 == WM_LBUTTONDBLCLK => Some(Command::Show),
        _ if msg == taskbar() && msg != 0 => Some(Command::Restore),
        _ => None,
    };
    if let Some(command) = command {
        if !super::hotkeys::is_editing()
            || matches!(
                command,
                Command::Show | Command::Menu | Command::Restore | Command::Exit
            )
        {
            queue(command);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
fn create_icon(mode: Mode) -> Result<HICON, String> {
    let mut pixels = vec![0u8; 32 * 32 * 4];
    let glyph = match mode {
        Mode::Device => [30u8, 17, 17, 17, 17, 17, 30],
        Mode::Application => [14, 17, 17, 31, 17, 17, 17],
        Mode::Browser => [30, 17, 17, 30, 17, 17, 30],
        Mode::Stopped => [15, 16, 16, 14, 1, 1, 30],
    };
    let color = [0u8, 90, 192, 255]; // BGRA: オレンジ #C05A00
    for y in 0usize..32 {
        for x in 0usize..32 {
            let on = match (x.checked_sub(6), y.checked_sub(2)) {
                (Some(x), Some(y)) if x / 4 < 5 && y / 4 < 7 => {
                    glyph[y / 4] & (1 << (4 - x / 4)) != 0
                }
                _ => false,
            };
            pixels[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4].copy_from_slice(if on {
                &[255, 255, 255, 255]
            } else {
                &color
            });
        }
    }
    unsafe {
        CreateIcon(None, 32, 32, 1, 32, [0u8; 128].as_ptr(), pixels.as_ptr())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod icon_tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::*;

    #[test]
    fn stopped_icon_renders_an_upright_s_on_orange_at_tray_and_window_sizes() {
        unsafe {
            let icon = create_icon(Mode::Stopped).unwrap();
            let dc = CreateCompatibleDC(None);
            let bitmap = CreateBitmap(32, 32, 1, 32, None);
            assert!(!dc.is_invalid() && !bitmap.is_invalid());
            let previous = SelectObject(dc, bitmap.into());
            let mut samples = Vec::new();
            for size in [16, 32] {
                DrawIconEx(dc, 0, 0, icon, size, size, 0, None, DI_NORMAL).unwrap();
                // Sは上半分の左側、下半分の右側が白い。上下反転すると2に見える。
                samples.push([
                    GetPixel(dc, 8 * size / 32, 12 * size / 32).0,
                    GetPixel(dc, 24 * size / 32, 12 * size / 32).0,
                    GetPixel(dc, 8 * size / 32, 20 * size / 32).0,
                    GetPixel(dc, 24 * size / 32, 20 * size / 32).0,
                    GetPixel(dc, 0, 0).0,
                ]);
            }
            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            DestroyIcon(icon).unwrap();
            for sample in samples {
                assert_eq!(sample, [0xffffff, 0x005ac0, 0x005ac0, 0xffffff, 0x005ac0]);
            }
        }
    }
}

pub(super) struct Tray {
    hwnd: HWND,
    icon: HICON,
    icons: [HICON; 4],
    mode: Mode,
    data: NOTIFYICONDATAW,
    attached: bool,
    keys: Option<KeyRegistration>,
    warning: String,
    _class: WindowClass,
}
impl Tray {
    pub(super) fn new(hotkeys: &[Hotkey; HOTKEY_COUNT]) -> Result<Self, String> {
        let class = WindowClass::register(CLASS, Some(resident_proc), HBRUSH::default())?;
        unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide(CLASS).as_ptr()),
                PCWSTR(wide(&title()).as_ptr()),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(class.instance()),
                None,
            )
            .map_err(|error| error.to_string())?;
            let mut tray = Self {
                hwnd,
                icon: HICON::default(),
                icons: [HICON::default(); 4],
                mode: Mode::Stopped,
                data: NOTIFYICONDATAW::default(),
                attached: false,
                keys: None,
                warning: String::new(),
                _class: class,
            };
            for (index, mode) in [
                Mode::Device,
                Mode::Application,
                Mode::Browser,
                Mode::Stopped,
            ]
            .into_iter()
            .enumerate()
            {
                tray.icons[index] = create_icon(mode)?;
            }
            tray.icon = tray.icons[3];
            tray.data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: CALLBACK,
                hIcon: tray.icon,
                ..Default::default()
            };
            let (keys, warning) = KeyRegistration::new(hwnd, hotkeys)?;
            tray.keys = Some(keys);
            tray.warning = warning;
            if !Shell_NotifyIconW(NIM_ADD, &tray.data).as_bool() {
                return Err("トレイの登録に失敗しました".into());
            }
            tray.attached = true;
            Ok(tray)
        }
    }
    pub(super) fn warning(&self) -> &str {
        &self.warning
    }
    pub(super) fn prepare_hotkeys(
        &mut self,
        next: &[Hotkey; HOTKEY_COUNT],
    ) -> Result<PreparedHotkeys, String> {
        self.keys
            .as_mut()
            .ok_or("キーの登録先は終了しています")?
            .prepare(next)
    }
    pub(super) fn commit_hotkeys(&mut self, prepared: PreparedHotkeys) -> Result<(), String> {
        self.keys
            .as_ref()
            .ok_or("キー監視は終了しています")?
            .commit(prepared)?;
        self.warning.clear();
        Ok(())
    }
    pub(super) fn set_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.icon = self.icons[match mode {
            Mode::Device => 0,
            Mode::Application => 1,
            Mode::Browser => 2,
            Mode::Stopped => 3,
        }];
        self.data.hIcon = self.icon;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
    }
    pub(super) fn icon(&self) -> HICON {
        self.icon
    }
    pub(super) fn update_tip(&mut self, tip: &str) {
        self.data.szTip.fill(0);
        for (target, source) in self.data.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
            *target = source;
        }
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
    }
    pub(super) fn restore(&mut self) {
        if unsafe { Shell_NotifyIconW(NIM_ADD, &self.data).as_bool() } {
            self.attached = true;
        }
    }
    pub(super) fn stop_fallback(&mut self) {
        self.keys = None;
    }
    pub(super) fn detach(&mut self) {
        unsafe {
            if self.attached {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
                self.attached = false;
            }
        }
        self.keys = None;
    }
    #[cfg(test)]
    pub(super) fn hwnd(&self) -> HWND {
        self.hwnd
    }
    #[cfg(test)]
    pub(super) fn stop_key_available(&self) -> bool {
        self.keys.as_ref().is_some()
    }
    pub(super) fn show_menu(&self, settings: &Settings, editing: bool) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            for (command, label, checked) in [
                (
                    Command::Device,
                    format!("再生デバイス\t{}", settings.hotkeys[0].label()),
                    settings.mode == Mode::Device,
                ),
                (
                    Command::Application,
                    format!("アプリ\t{}", settings.hotkeys[1].label()),
                    settings.mode == Mode::Application,
                ),
                (
                    Command::Browser,
                    format!("ブラウザ\t{}", settings.hotkeys[2].label()),
                    settings.mode == Mode::Browser,
                ),
                (
                    Command::Stopped,
                    format!("停止\t{}", settings.hotkeys[3].label()),
                    settings.mode == Mode::Stopped,
                ),
                (
                    Command::Output1,
                    format!(
                        "出力1 {}",
                        if settings.outputs[0].enabled {
                            "ON"
                        } else {
                            "OFF"
                        }
                    ),
                    settings.outputs[0].enabled,
                ),
                (
                    Command::Output2,
                    format!(
                        "出力2 {}",
                        if settings.outputs[1].enabled {
                            "ON"
                        } else {
                            "OFF"
                        }
                    ),
                    settings.outputs[1].enabled,
                ),
                (Command::Show, "設定".into(), false),
                (Command::MenuExit, "終了".into(), false),
            ] {
                if matches!(command, Command::Output1 | Command::Show) {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
                let mut flags = MF_STRING;
                let input_mode = match command {
                    Command::Device => Some(Mode::Device),
                    Command::Application => Some(Mode::Application),
                    Command::Browser => Some(Mode::Browser),
                    _ => None,
                };
                if input_mode.is_some_and(|mode| !settings.mode_enabled(mode)) {
                    flags |= MF_GRAYED;
                }
                if checked {
                    flags |= MF_CHECKED;
                }
                if editing && command != Command::Show {
                    flags |= MF_GRAYED;
                }
                let _ = AppendMenuW(menu, flags, command as usize, PCWSTR(wide(&label).as_ptr()));
            }
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(self.hwnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            )
            .0 as usize;
            let _ = DestroyMenu(menu);
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            if let Some(command) = Command::from_id(selected) {
                queue(command);
            }
        }
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        self.stop_fallback();
        self.detach();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            for icon in self.icons {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(icon);
                }
            }
        }
    }
}
