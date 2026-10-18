use super::model::KeyCombination;

pub trait ShortcutRegistrar: Send + 'static {
    fn register(
        &mut self,
        registration_id: i32,
        combination: KeyCombination,
    ) -> std::result::Result<(), String>;
    fn unregister(&mut self, registration_id: i32);
    fn poll_triggered(&mut self) -> Vec<i32>;
}

pub fn platform_registrar() -> Box<dyn ShortcutRegistrar> {
    Box::new(PlatformShortcutRegistrar::default())
}

#[derive(Default)]
struct PlatformShortcutRegistrar;

#[cfg(windows)]
impl ShortcutRegistrar for PlatformShortcutRegistrar {
    fn register(
        &mut self,
        registration_id: i32,
        combination: KeyCombination,
    ) -> std::result::Result<(), String> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
            MOD_WIN,
        };

        let mut modifiers = HOT_KEY_MODIFIERS(0);
        if combination.ctrl {
            modifiers |= MOD_CONTROL;
        }
        if combination.shift {
            modifiers |= MOD_SHIFT;
        }
        if combination.alt {
            modifiers |= MOD_ALT;
        }
        if combination.win {
            modifiers |= MOD_WIN;
        }
        modifiers |= MOD_NOREPEAT;
        unsafe { RegisterHotKey(None, registration_id, modifiers, combination.key.0 as u32) }
            .map_err(|_| "SHORTCUT_REGISTRATION_FAILED: Windows rejected the shortcut".into())
    }

    fn unregister(&mut self, registration_id: i32) {
        use windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey;
        let _ = unsafe { UnregisterHotKey(None, registration_id) };
    }

    fn poll_triggered(&mut self) -> Vec<i32> {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY};
        let mut triggered = Vec::new();
        loop {
            let mut message = MSG::default();
            if !unsafe { PeekMessageW(&mut message, None, WM_HOTKEY, WM_HOTKEY, PM_REMOVE) }
                .as_bool()
            {
                break;
            }
            triggered.push(message.wParam.0 as i32);
        }
        triggered
    }
}

#[cfg(not(windows))]
impl ShortcutRegistrar for PlatformShortcutRegistrar {
    fn register(
        &mut self,
        _registration_id: i32,
        _combination: KeyCombination,
    ) -> std::result::Result<(), String> {
        Err("SHORTCUT_REGISTRATION_FAILED: global shortcuts require Windows".into())
    }

    fn unregister(&mut self, _registration_id: i32) {}

    fn poll_triggered(&mut self) -> Vec<i32> {
        Vec::new()
    }
}
