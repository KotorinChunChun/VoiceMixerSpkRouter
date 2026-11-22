//! 実音声・管理IPC・グローバルキーを作らない説明書撮影入口。
use super::{
    commands::{self, Command},
    overlay::{DisplayState, OverlayWindow},
    settings::SettingsWindow,
};
use crate::{
    config::{OverlayMode, Settings},
    state::Phase,
};
use std::{path::Path, time::Duration};
use windows::Win32::UI::WindowsAndMessaging::*;
pub fn run(output: &Path) -> Result<(), String> {
    std::fs::create_dir(output).map_err(|e| e.to_string())?;
    commands::clear();
    let mut settings = Settings::default();
    settings.browser_token = "documentation-sample-only".into();
    settings.overlay_mode = OverlayMode::Always;
    let overlay = OverlayWindow::new()?;
    overlay.update(&DisplayState::new(&settings, Phase::Stopped, "", "", false));
    let mut editor = SettingsWindow::new(&settings, HICON::default(), "")?;
    editor.show();
    loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                if !editor.dialog_message(&msg) {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        for (command, _) in commands::drain() {
            match command {
                Command::Exit | Command::MenuExit | Command::Cancel => return Ok(()),
                Command::Show => editor.show(),
                Command::BasicPage => editor.set_page(0),
                Command::KeyPage => editor.set_page(1),
                Command::ResetPage => editor.reset_page(),
                Command::KeyChanged => editor.key_selection_changed(),
                Command::InputChanged => editor.input_selection_changed(),
                Command::Save => match editor.read() {
                    Ok(next) => {
                        std::fs::write(
                            output.join("settings.json"),
                            serde_json::to_string_pretty(&next).map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                        settings = next;
                        editor.applied(&settings);
                        editor.message("設定を保存しました。");
                    }
                    Err(error) => editor.message(&error),
                },
                _ => editor.message("撮影用のため外部接続・音声転送は実行しません。"),
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}
