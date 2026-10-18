//! 複数のネイティブ画面で共用する文字列・フォント・ウィンドウクラス。
use crate::config::{APP_NAME, APP_VERSION};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HINSTANCE,
        Graphics::Gdi::{
            CreateFontW, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, HBRUSH, HFONT,
            OUT_DEFAULT_PRECIS,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            LoadCursorW, MessageBoxW, RegisterClassW, UnregisterClassW, CS_DBLCLKS, IDC_ARROW,
            MB_ICONERROR, MB_OK, WNDCLASSW, WNDPROC,
        },
    },
};

pub(super) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
pub(super) fn title() -> String {
    format!("{APP_NAME} v{APP_VERSION}")
}
pub(super) fn error(message: &str) {
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide(message).as_ptr()),
            PCWSTR(wide(&title()).as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
pub(super) unsafe fn font(size: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -size,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            PCWSTR(wide("Yu Gothic UI").as_ptr()),
        )
    }
}

/// 対応するウィンドウを破棄した後に解除する。所有する画面の最後のフィールドに置く。
pub(super) struct WindowClass {
    name: &'static str,
    instance: HINSTANCE,
}
impl WindowClass {
    pub(super) fn register(
        name: &'static str,
        callback: WNDPROC,
        background: HBRUSH,
    ) -> Result<Self, String> {
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None)
                .map_err(|error| error.to_string())?
                .into();
            let class_name = wide(name);
            let class = WNDCLASSW {
                style: CS_DBLCLKS,
                lpfnWndProc: callback,
                hInstance: instance,
                lpszClassName: PCWSTR(class_name.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).map_err(|error| error.to_string())?,
                hbrBackground: background,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_thread().to_string());
            }
            Ok(Self { name, instance })
        }
    }
    pub(super) fn instance(&self) -> HINSTANCE {
        self.instance
    }
}
impl Drop for WindowClass {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterClassW(PCWSTR(wide(self.name).as_ptr()), Some(self.instance));
        }
    }
}
