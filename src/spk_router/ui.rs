//! 再生音声転送UIの統括。画面、トレイ、キー検出の寿命と操作順を管理する。
#[path = "ui/browser_help.rs"]
mod browser_help;
#[path = "ui/commands.rs"]
mod commands;
#[path = "hotkeys.rs"]
mod hotkeys;
#[path = "ui/native.rs"]
mod native;
#[path = "ui/overlay.rs"]
mod overlay;
#[path = "ui/settings.rs"]
mod settings;
#[path = "ui/tray.rs"]
mod tray;

use crate::{
    config::Settings,
    key_binding::{Hotkey, HOTKEY_COUNT},
    state::{Action, Phase},
};
pub use commands::ExistingCommand;
use commands::{queue, Command};
use overlay::{DisplayState, OverlayWindow};
use settings::SettingsWindow;
pub(crate) use tray::PreparedHotkeys;
use tray::Tray;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

pub fn send_existing(command: ExistingCommand) -> Result<bool, String> {
    tray::send_existing(command)
}
pub fn error(message: &str) {
    native::error(message);
}

pub struct Ui {
    // Dropの明示処理後は、この順序で各ウィンドウと借用アイコンを解放する。
    editor: Option<SettingsWindow>,
    overlay: OverlayWindow,
    tray: Tray,
    settings: Settings,
    phase: Phase,
    message: String,
    applying: bool,
    operation_error: String,
}
impl Ui {
    pub fn new(settings: &Settings) -> Result<Self, String> {
        commands::clear();
        hotkeys::set_editing(false);
        let mut tray = Tray::new(&effective_hotkeys(settings))?;
        tray.set_mode(settings.mode);
        let overlay = OverlayWindow::new()?;
        let mut ui = Self {
            editor: None,
            overlay,
            tray,
            settings: settings.clone(),
            phase: Phase::Stopped,
            message: String::new(),
            applying: false,
            operation_error: String::new(),
        };
        ui.redraw();
        Ok(ui)
    }
    pub fn update(&mut self, settings: &Settings, phase: Phase, message: &str) {
        self.settings = settings.clone();
        if let Some(editor) = &mut self.editor {
            editor.sync_runtime(settings);
        }
        self.tray.set_mode(settings.mode);
        self.phase = phase;
        self.message = message.into();
        self.redraw();
    }
    pub fn prepare_hotkeys(&mut self, settings: &Settings) -> Result<PreparedHotkeys, String> {
        self.tray.prepare_hotkeys(&effective_hotkeys(settings))
    }
    pub fn commit_hotkeys(&mut self, prepared: PreparedHotkeys) -> Result<(), String> {
        self.tray.commit_hotkeys(prepared)
    }
    pub fn settings_saved(&mut self, settings: &Settings) {
        self.settings = settings.clone();
        if let Some(editor) = &mut self.editor {
            editor.applied(settings);
        }
        self.redraw();
    }
    pub fn set_applying(&mut self, applying: bool) {
        self.applying = applying;
        if let Some(editor) = &self.editor {
            editor.set_applying(applying);
        }
    }
    pub fn update_meters(&mut self, snapshot: Option<&crate::telemetry::Snapshot>) {
        self.overlay.update_meters(snapshot, self.settings.mode);
    }
    pub fn clear_operation_error(&mut self) {
        self.operation_error.clear();
    }
    pub fn settings_error(&mut self, message: &str) {
        if let Some(editor) = &self.editor {
            editor.message(message);
        } else {
            self.operation_error = message.into();
        }
    }
    fn redraw(&mut self) {
        let state = DisplayState::new(
            &self.settings,
            self.phase,
            &self.message,
            if self.operation_error.is_empty() {
                self.tray.warning()
            } else {
                &self.operation_error
            },
            self.editor.is_some(),
        );
        if self.overlay.update(&state) {
            self.tray.update_tip(&state.tooltip());
        }
    }
    pub fn management_stop_block(&self) -> Option<&'static str> {
        if self.applying {
            Some("設定を適用中です。完了後にやり直してください。")
        } else if self.editor.is_some() {
            Some("設定画面を閉じてからやり直してください。未保存の編集は保持します。")
        } else {
            None
        }
    }
    pub fn settings_is_open(&self) -> bool {
        self.editor.is_some()
    }
    pub fn show_settings(&mut self) {
        if self.editor.is_none() {
            hotkeys::set_editing(true);
            match SettingsWindow::new(&self.settings, self.tray.icon(), self.tray.warning()) {
                Ok(window) => self.editor = Some(window),
                Err(message) => {
                    hotkeys::set_editing(false);
                    error(&message);
                    return;
                }
            }
        }
        if let Some(editor) = &self.editor {
            editor.set_applying(self.applying);
            editor.show();
        }
        self.redraw();
    }
    fn pump_messages(&self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    queue(Command::Exit);
                    continue;
                }
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.dialog_message(&msg))
                {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    pub fn poll(&mut self) -> Vec<Action> {
        self.pump_messages();
        let mut actions = Vec::new();
        for (command, shortcut) in commands::drain() {
            match command {
                Command::Show => self.show_settings(),
                Command::Menu => self.tray.show_menu(&self.settings, self.editor.is_some()),
                Command::Restore => self.tray.restore(),
                Command::Cancel if !self.applying => {
                    hotkeys::set_editing(false);
                    self.editor = None;
                    self.redraw();
                }
                Command::Save if !self.applying => {
                    if let Some(editor) = &self.editor {
                        match editor.read() {
                            Ok(mut settings) => {
                                // 状態更新はdraftに書き戻さず、保存時のmodeだけ現行値を使う。
                                settings.mode = self.settings.mode;
                                actions.push(Action::Configure(Box::new(settings)));
                            }
                            Err(message) => editor.message(&message),
                        }
                    }
                }
                Command::BasicPage => {
                    if let Some(editor) = &self.editor {
                        editor.set_page(0);
                    }
                }
                Command::KeyPage => {
                    if let Some(editor) = &self.editor {
                        editor.set_page(1);
                    }
                }
                Command::Refresh => {
                    if let Some(editor) = &mut self.editor {
                        if let Err(message) = editor.refresh() {
                            editor.message(&message);
                        }
                    }
                }
                Command::AppChanged => {
                    if let Some(editor) = &self.editor {
                        editor.app_path();
                    }
                }
                Command::KeyChanged => {
                    if let Some(editor) = &self.editor {
                        editor.key_selection_changed();
                    }
                }
                Command::InputChanged => {
                    if let Some(editor) = &self.editor {
                        editor.input_selection_changed();
                    }
                }
                Command::OpenRepository => {
                    let owner = self.editor.as_ref().map(|editor| editor.hwnd());
                    if let Err(message) =
                        browser_help::open_url(owner, browser_help::REPOSITORY_URL)
                    {
                        if let Some(editor) = &self.editor {
                            editor.message(&message);
                        }
                    }
                }
                Command::BrowserInstallHelp | Command::BrowserSetupHelp => {
                    let owner = self.editor.as_ref().map(|editor| editor.hwnd());
                    if let Err(message) =
                        browser_help::open(owner, matches!(command, Command::BrowserSetupHelp))
                    {
                        if let Some(editor) = &self.editor {
                            editor.message(&message);
                        }
                    }
                }
                _ => {
                    if let Some(action) = command.action(self.editor.is_some() && !shortcut) {
                        actions.push(action);
                    }
                }
            }
        }
        actions
    }
}

fn effective_hotkeys(settings: &Settings) -> [Hotkey; HOTKEY_COUNT] {
    let mut keys = settings.hotkeys.clone();
    for (key, available) in keys.iter_mut().zip(settings.input_enabled) {
        key.enabled &= available;
    }
    keys
}
impl Drop for Ui {
    fn drop(&mut self) {
        // フックを先に停止し、設定窓が借用するアイコンは最後まで生存させる。
        hotkeys::set_editing(false);
        self.tray.stop_fallback();
        self.editor = None;
        self.tray.detach();
        // この後overlay、tray（通知窓→アイコン→クラス）の順でDropされる。
    }
}

#[cfg(test)]
mod tests {
    use super::{commands::Command, native::title, tray::CONTROL, Ui};
    use crate::{
        config::{Mode, Settings},
        state::{Action, Phase},
    };
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE},
    };
    #[test]
    fn disabled_sources_do_not_intercept_keys_and_saved_assignments_are_preserved() {
        let mut settings = Settings::default();
        settings.input_enabled = [false, true, false];
        for key in &mut settings.hotkeys {
            key.mode = crate::key_binding::KeyMode::Override;
        }
        let effective = super::effective_hotkeys(&settings);
        assert!(!effective[0].is_assigned());
        assert!(effective[1].is_assigned());
        assert!(!effective[2].is_assigned());
        assert!(effective[3].is_assigned());
        assert!(settings.hotkeys[0].is_assigned());
    }
    #[test]
    #[ignore = "設定を開いたまま実通知窓でショートカットと下書き保持を検証するGUI試験"]
    fn shortcuts_while_editing_preserve_drafts_and_follow_live_display() {
        use super::{hotkeys, tray::SHORTCUT};
        use crate::{config::OverlayMode, state::Transition};
        let mut settings = Settings::default();
        let mut ui = Ui::new(&settings).unwrap();
        ui.show_settings();
        let window = ui.editor.as_ref().unwrap().hwnd();
        // キー監視が配送する通知と同じ経路。合成キーの試験とは区別する。
        for command in [
            Command::Device,
            Command::Application,
            Command::Browser,
            Command::Browser,
            Command::Stopped,
            Command::ToggleOverlay,
        ] {
            unsafe {
                PostMessageW(
                    Some(ui.tray.hwnd()),
                    SHORTCUT,
                    WPARAM(command as usize),
                    LPARAM(hotkeys::generation() as isize),
                )
                .unwrap();
            }
            let mut actions = ui.poll();
            assert_eq!(actions.len(), 1, "設定を開いたままキーを受け付ける");
            let Transition::Continue(next) = actions.remove(0).transition(&settings) else {
                panic!("設定変更を期待");
            };
            settings = next;
            ui.update(&settings, Phase::Stopped, "");
            assert_eq!(ui.editor.as_ref().unwrap().hwnd(), window);
        }
        assert_eq!(settings.mode, Mode::Stopped);
        assert_eq!(settings.overlay_mode, OverlayMode::Off);
        assert_eq!(
            ui.editor.as_ref().unwrap().read().unwrap().overlay_mode,
            OverlayMode::Off
        );

        ui.editor
            .as_ref()
            .unwrap()
            .edit_for_test("未適用のストリーム", true);
        for display in [OverlayMode::Brief, OverlayMode::Always, OverlayMode::Off] {
            settings.overlay_mode = display;
            settings.mode = Mode::Browser;
            ui.update(&settings, Phase::Waiting, "待機");
            let draft = ui.editor.as_ref().unwrap().read().unwrap();
            assert_eq!(draft.browser_stream_key, "未適用のストリーム");
            assert_eq!(draft.overlay_mode, OverlayMode::Always);
        }
        super::commands::queue(Command::Save);
        let mut actions = ui.poll();
        let Action::Configure(draft) = actions.remove(0) else {
            panic!("適用を期待");
        };
        assert_eq!(draft.mode, Mode::Browser, "最新の取得元を巻き戻さない");
        ui.set_applying(true);
        unsafe {
            PostMessageW(
                Some(ui.tray.hwnd()),
                SHORTCUT,
                WPARAM(Command::Stopped as usize),
                LPARAM(hotkeys::generation() as isize),
            )
            .unwrap();
            PostMessageW(
                Some(ui.tray.hwnd()),
                SHORTCUT,
                WPARAM(Command::Output1 as usize),
                LPARAM(hotkeys::generation() as isize),
            )
            .unwrap();
            PostMessageW(
                Some(ui.tray.hwnd()),
                SHORTCUT,
                WPARAM(Command::Device as usize),
                LPARAM(hotkeys::generation().wrapping_sub(1) as isize),
            )
            .unwrap();
        }
        let actions = ui.poll();
        assert_eq!(
            actions.len(),
            1,
            "適用中も停止を受け付け、不正・旧割当の通知は捨てる"
        );
        assert!(matches!(actions[0], Action::Select(Mode::Stopped)));
        ui.set_applying(false);
        ui.settings_saved(&draft);
        let mut live = (*draft).clone();
        live.overlay_mode = OverlayMode::Brief;
        ui.update(&live, Phase::Waiting, "");
        assert_eq!(
            ui.editor.as_ref().unwrap().read().unwrap().overlay_mode,
            OverlayMode::Brief,
            "適用成功後は未編集として表示キーへ追従する"
        );
    }
    #[test]
    #[ignore = "Windows のトレイと設定窓を作る実機 GUI 試験"]
    fn settings_roundtrip_keeps_edits_during_status_updates() {
        let settings = Settings::default();
        let mut ui = Ui::new(&settings).unwrap();
        assert!(ui.tray.stop_key_available());
        ui.show_settings();
        let editor = ui.editor.as_ref().unwrap();
        editor.edit_for_test("edited-stream", false);
        ui.update(&settings, Phase::Running, "検証中");
        let edited = ui.editor.as_ref().unwrap().read().unwrap();
        assert_eq!(edited.browser_stream_key, "edited-stream");
        assert_eq!(edited.overlay_mode, crate::config::OverlayMode::Off);
        assert_eq!(ui.editor.as_ref().unwrap().caption(), title());
        ui.editor.as_mut().unwrap().refresh().unwrap();
        assert_eq!(ui.editor.as_ref().unwrap().read().unwrap(), edited);
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(10) {
            ui.poll();
            // 停止時はdetailが空文字。実際のWM_PAINT処理まで通して確認する。
            ui.update(&settings, Phase::Stopped, "");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let painted_title = ui.overlay.painted_title();
        assert_eq!(painted_title, "停止");
        assert_eq!(ui.editor.as_ref().unwrap().read().unwrap(), edited);
        unsafe {
            PostMessageW(
                Some(ui.tray.hwnd()),
                CONTROL,
                WPARAM(Command::SetDevice as usize),
                LPARAM(0),
            )
            .unwrap();
            PostMessageW(
                Some(ui.tray.hwnd()),
                CONTROL,
                WPARAM(Command::Output1 as usize),
                LPARAM(0),
            )
            .unwrap();
            PostMessageW(
                Some(ui.tray.hwnd()),
                CONTROL,
                WPARAM(Command::SetStopped as usize),
                LPARAM(0),
            )
            .unwrap();
        }
        let actions = ui.poll();
        assert!(actions.is_empty());
        let editor = ui.editor.as_ref().unwrap().hwnd();
        unsafe {
            PostMessageW(Some(editor), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
        }
        assert!(ui.poll().is_empty());
        assert!(ui.editor.is_none());
        unsafe {
            PostMessageW(
                Some(ui.tray.hwnd()),
                CONTROL,
                WPARAM(Command::SetBrowser as usize),
                LPARAM(0),
            )
            .unwrap();
        }
        let actions = ui.poll();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::Set(Mode::Browser)));
        // 設定窓を閉じた際にクラスも解除でき、同じ常駐内で開き直せること。
        ui.show_settings();
        assert_eq!(ui.editor.as_ref().unwrap().caption(), title());
        drop(ui);
        // HWND、キー、クラスの所有を解放してから同じUIを再構築できること。
        let reopened = Ui::new(&settings).unwrap();
        assert!(reopened.tray.stop_key_available());
    }

    #[test]
    #[ignore = "Windows の設定窓で保存要求と応答を確認する実機 GUI 試験"]
    fn settings_draft_survives_save_request_and_error_until_success() {
        let settings = Settings::default();
        let mut ui = Ui::new(&settings).unwrap();
        ui.show_settings();
        let mut binding = settings.hotkeys[0].clone();
        binding.ctrl = true;
        binding.key = "F21".into();
        ui.editor.as_ref().unwrap().set_hotkey_for_test(0, &binding);
        super::commands::queue(Command::Save);
        let mut actions = ui.poll();
        assert_eq!(actions.len(), 1);
        let Action::Configure(draft) = actions.remove(0) else {
            panic!("保存要求を期待");
        };
        assert!(
            ui.editor.is_some(),
            "保存結果を受け取るまで編集窓を閉じない"
        );
        assert_eq!(draft.hotkeys[0], binding);
        ui.settings_error("キーが競合しています");
        ui.update(&settings, Phase::Stopped, "停止");
        ui.editor.as_mut().unwrap().refresh().unwrap();
        assert_eq!(
            ui.editor.as_ref().unwrap().read().unwrap().hotkeys[0],
            binding
        );
        ui.settings_saved(&draft);
        assert!(ui.editor.is_some(), "適用後も編集を続けられる");
        assert_eq!(ui.settings.hotkeys[0], binding);
        super::commands::queue(Command::Cancel);
        assert!(ui.poll().is_empty());
        assert!(ui.editor.is_none());
        ui.show_settings();
        assert_eq!(
            ui.editor.as_ref().unwrap().read().unwrap().hotkeys[0],
            binding
        );
    }
}

#[cfg(test)]
mod observer_tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    };
    #[test]
    #[ignore = "実Win32キー予約と透過監視を併用する実機試験"]
    fn observer_does_not_reserve_or_prevent_another_app_hotkey() {
        struct Reserved;
        impl Drop for Reserved {
            fn drop(&mut self) {
                unsafe {
                    let _ = UnregisterHotKey(None, 0x6a77);
                }
            }
        }
        let mut free = None;
        for number in 13..=24 {
            if unsafe {
                RegisterHotKey(
                    None,
                    0x6a77,
                    MOD_CONTROL | MOD_SHIFT | MOD_ALT | MOD_NOREPEAT,
                    0x70 + number - 1,
                )
            }
            .is_ok()
            {
                unsafe {
                    UnregisterHotKey(None, 0x6a77).unwrap();
                }
                free = Some(number);
                break;
            }
        }
        let number = free.expect("試験用の空きキーが必要");
        let mut settings = Settings::default();
        for key in &mut settings.hotkeys {
            key.enabled = false;
        }
        settings.hotkeys[0].enabled = true;
        settings.hotkeys[0].ctrl = true;
        settings.hotkeys[0].shift = true;
        settings.hotkeys[0].key = format!("F{number}");
        let ui = Ui::new(&settings).unwrap();
        unsafe {
            RegisterHotKey(
                None,
                0x6a77,
                MOD_CONTROL | MOD_SHIFT | MOD_ALT | MOD_NOREPEAT,
                0x70 + number - 1,
            )
            .unwrap();
        }
        let reserved = Reserved;
        drop(ui);
        drop(reserved);
    }
}
