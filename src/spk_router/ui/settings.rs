//! 設定欄と未保存の編集内容を所有する。未編集の表示方法だけ実行状態に追従する。
use super::{
    commands::{queue, Command},
    native::{font, title, wide, WindowClass},
};
use crate::{
    catalog::{application_choices, device_choices},
    config::{Mode, OverlayMode, Settings},
    key_binding::{key_choices, Hotkey, KeyMode, HOTKEY_COUNT, HOTKEY_LABELS},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::*,
        UI::{Controls::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};
const SETTINGS_CLASS: &str = "VoiceMixerSpkSettingsV1";
const SETTINGS_WIDTH: i32 = 840;
const SETTINGS_HEIGHT: i32 = 580;
const CONTENT_OFFSET: i32 = 40;
const HOTKEY_BASE: usize = 1000;
const APPLICATION_CONTROL: usize = 302;
const TAB: usize = 12;
const BROWSER_HELP: usize = 350;
const BROWSER_SETUP_HELP: usize = 352;
const REPOSITORY_LINK: usize = 353;
const SEPARATOR_STYLE: u32 = 0x10; // Win32 SS_ETCHEDHORZ
unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_DRAWITEM if l.0 != 0 => {
            let item = unsafe { &*(l.0 as *const DRAWITEMSTRUCT) };
            if matches!(
                item.CtlID as usize,
                BROWSER_HELP | BROWSER_SETUP_HELP | REPOSITORY_LINK
            ) {
                draw_help_link(item);
                return LRESULT(1);
            }
        }
        WM_SETCURSOR
            if matches!(
                unsafe { GetDlgCtrlID(HWND(w.0 as *mut _)) } as usize,
                BROWSER_HELP | BROWSER_SETUP_HELP | REPOSITORY_LINK
            ) =>
        {
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(cursor));
                }
            }
            return LRESULT(1);
        }
        WM_NOTIFY if l.0 != 0 => {
            let header = unsafe { &*(l.0 as *const NMHDR) };
            if header.idFrom == TAB && header.code == TCN_SELCHANGE {
                match unsafe { SendMessageW(header.hwndFrom, TCM_GETCURSEL, None, None).0 } {
                    0 => queue(Command::BasicPage),
                    1 => queue(Command::KeyPage),
                    _ => {}
                }
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            queue(Command::Cancel);
            return LRESULT(0);
        }
        WM_COMMAND => {
            let id = w.0 & 0xffff;
            let notification = w.0 >> 16;
            if notification == 0 {
                if (345..=347).contains(&id) {
                    queue(Command::InputChanged);
                    return LRESULT(0);
                }
                if (HOTKEY_BASE..HOTKEY_BASE + HOTKEY_COUNT * 10).contains(&id)
                    && (id - HOTKEY_BASE) % 10 == 0
                {
                    queue(Command::KeyChanged);
                    return LRESULT(0);
                }
                let command = match id {
                    1 => Some(Command::Save),
                    2 => Some(Command::Cancel),
                    BROWSER_HELP => Some(Command::BrowserInstallHelp),
                    BROWSER_SETUP_HELP => Some(Command::BrowserSetupHelp),
                    REPOSITORY_LINK => Some(Command::OpenRepository),
                    _ => Command::from_id(id),
                };
                if let Some(command) = command {
                    queue(command);
                }
            } else if notification == CBN_SELCHANGE as usize && id == APPLICATION_CONTROL {
                queue(Command::AppChanged);
            } else if notification == CBN_SELCHANGE as usize
                && (HOTKEY_BASE + 5..HOTKEY_BASE + HOTKEY_COUNT * 10).contains(&id)
                && (id - HOTKEY_BASE - 5) % 10 == 0
            {
                if let Ok(combo) = unsafe { GetDlgItem(Some(hwnd), id as i32) } {
                    if selected(combo) == 0 {
                        if let Ok(enabled) = unsafe { GetDlgItem(Some(hwnd), id as i32 - 5) } {
                            check(enabled, false);
                        }
                    }
                }
                queue(Command::KeyChanged);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
fn draw_help_link(item: &DRAWITEMSTRUCT) {
    unsafe {
        let saved = SaveDC(item.hDC);
        FillRect(item.hDC, &item.rcItem, GetSysColorBrush(COLOR_BTNFACE));
        let font = SendMessageW(item.hwndItem, WM_GETFONT, None, None);
        SelectObject(item.hDC, HGDIOBJ(font.0 as *mut _));
        SetBkMode(item.hDC, TRANSPARENT);
        let color = if item.itemState.0 & ODS_DISABLED.0 != 0 {
            COLORREF(GetSysColor(COLOR_GRAYTEXT))
        } else {
            COLORREF(0x00aa4400)
        };
        SetTextColor(item.hDC, color);
        let mut value: Vec<u16> = text(item.hwndItem).encode_utf16().collect();
        let mut rect = item.rcItem;
        rect.left += 3;
        DrawTextW(
            item.hDC,
            &mut value,
            &mut rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
        let mut size = windows::Win32::Foundation::SIZE::default();
        let _ = GetTextExtentPoint32W(item.hDC, &value, &mut size);
        let pen = CreatePen(PS_SOLID, 1, color);
        let old = SelectObject(item.hDC, pen.into());
        let y = (rect.top + rect.bottom + size.cy) / 2 - 2;
        let _ = MoveToEx(item.hDC, rect.left, y, None);
        let _ = LineTo(item.hDC, rect.left + size.cx, y);
        SelectObject(item.hDC, old);
        let _ = DeleteObject(pen.into());
        if item.itemState.0 & ODS_FOCUS.0 != 0 {
            let _ = DrawFocusRect(item.hDC, &item.rcItem);
        }
        let _ = RestoreDC(item.hDC, saved);
    }
}
fn text(hwnd: HWND) -> String {
    unsafe {
        let mut buffer = vec![0u16; GetWindowTextLengthW(hwnd) as usize + 1];
        let len = GetWindowTextW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..len as usize])
    }
}
fn set_text(hwnd: HWND, value: &str) {
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(wide(value).as_ptr()));
    }
}
fn selected(hwnd: HWND) -> usize {
    unsafe { SendMessageW(hwnd, CB_GETCURSEL, None, None).0 as usize }
}
fn checked(hwnd: HWND) -> bool {
    unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 == 1 }
}
fn check(hwnd: HWND, value: bool) {
    unsafe {
        SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(value as usize)), None);
    }
}
fn combo_items(hwnd: HWND, values: impl IntoIterator<Item = String>, index: usize) {
    unsafe {
        SendMessageW(hwnd, CB_RESETCONTENT, None, None);
        for value in values {
            SendMessageW(
                hwnd,
                CB_ADDSTRING,
                None,
                Some(LPARAM(wide(&value).as_ptr() as isize)),
            );
        }
        SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(index)), None);
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            SendMessageW(
                hwnd,
                CB_SETDROPPEDWIDTH,
                Some(WPARAM((rect.right - rect.left) as usize)),
                None,
            );
        }
    }
}
pub(super) struct SettingsWindow {
    hwnd: HWND,
    font: HFONT,
    base: Settings,
    keys: [[HWND; 7]; HOTKEY_COUNT],
    devices: [HWND; 3],
    device_values: [Vec<(String, String)>; 3],
    app: HWND,
    app_detail: HWND,
    app_values: Vec<(PathBuf, String)>,
    outputs: [HWND; 2],
    inputs: [HWND; 3],
    input_controls: RefCell<[Vec<HWND>; 3]>,
    building_input: Cell<usize>,
    port: HWND,
    token: HWND,
    stream: HWND,
    overlay: HWND,
    overlay_edited: bool,
    startup: HWND,
    labels: [HWND; 2],
    pages: RefCell<[Vec<HWND>; 2]>,
    building_page: Cell<usize>,
    tab: HWND,
    applying: Cell<bool>,
    message: HWND,
    _class: WindowClass,
}
impl SettingsWindow {
    pub(super) fn new(settings: &Settings, icon: HICON, warning: &str) -> Result<Self, String> {
        unsafe {
            let class = WindowClass::register(
                SETTINGS_CLASS,
                Some(settings_proc),
                HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            )?;
            let instance = class.instance();
            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: SETTINGS_WIDTH,
                bottom: SETTINGS_HEIGHT,
            };
            AdjustWindowRectEx(&mut rect, style, false, WS_EX_CONTROLPARENT)
                .map_err(|e| e.to_string())?;
            let hwnd = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                PCWSTR(wide(SETTINGS_CLASS).as_ptr()),
                PCWSTR(wide(&title()).as_ptr()),
                style,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            let mut s = Self {
                hwnd,
                font: font(16),
                base: settings.clone(),
                keys: [[HWND::default(); 7]; HOTKEY_COUNT],
                devices: [HWND::default(); 3],
                device_values: std::array::from_fn(|_| Vec::new()),
                app: HWND::default(),
                app_detail: HWND::default(),
                app_values: Vec::new(),
                outputs: [HWND::default(); 2],
                inputs: [HWND::default(); 3],
                input_controls: RefCell::new(std::array::from_fn(|_| Vec::new())),
                building_input: Cell::new(3),
                port: HWND::default(),
                token: HWND::default(),
                stream: HWND::default(),
                overlay: HWND::default(),
                overlay_edited: false,
                startup: HWND::default(),
                labels: [HWND::default(); 2],
                pages: RefCell::new(std::array::from_fn(|_| Vec::new())),
                building_page: Cell::new(2),
                tab: HWND::default(),
                applying: Cell::new(false),
                message: HWND::default(),
                _class: class,
            };
            for size in [ICON_SMALL, ICON_BIG] {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(size as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
            }
            if !InitCommonControlsEx(&INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_TAB_CLASSES,
            })
            .as_bool()
            {
                return Err("設定タブを初期化できません".into());
            }
            s.tab = s.control(
                "SysTabControl32",
                "",
                TCS_FIXEDWIDTH | WS_CLIPSIBLINGS.0,
                TAB,
                10,
                12,
                820,
                476,
            )?;
            SendMessageW(s.tab, TCM_SETITEMSIZE, None, Some(LPARAM((32 << 16) | 145)));
            for (index, name) in ["基本設定", "キー設定"].iter().enumerate() {
                let mut name = wide(name);
                let item = TCITEMW {
                    mask: TCIF_TEXT,
                    pszText: PWSTR(name.as_mut_ptr()),
                    ..Default::default()
                };
                if SendMessageW(
                    s.tab,
                    TCM_INSERTITEMW,
                    Some(WPARAM(index)),
                    Some(LPARAM(&item as *const _ as isize)),
                )
                .0 == -1
                {
                    return Err("設定タブを作成できません".into());
                }
            }
            s.building_page.set(0);
            s.label("オーバーレイ表示", 20, 59, 165)?;
            s.overlay = s.combo(333, 190, 56, 622)?;
            combo_items(
                s.overlay,
                ["オフ", "数秒表示", "常時表示"].map(str::to_owned),
                match settings.overlay_mode {
                    OverlayMode::Off => 0,
                    OverlayMode::Brief => 1,
                    OverlayMode::Always => 2,
                },
            );
            s.label("ログイン時のモード", 20, 97, 165)?;
            s.startup = s.combo(335, 190, 94, 622)?;
            combo_items(
                s.startup,
                [
                    "前回のモード",
                    "再生デバイス",
                    "アプリ",
                    "ブラウザ",
                    "送信停止",
                ]
                .map(str::to_owned),
                match settings.startup_mode {
                    None => 0,
                    Some(Mode::Device) => 1,
                    Some(Mode::Application) => 2,
                    Some(Mode::Browser) => 3,
                    Some(Mode::Stopped) => 4,
                },
            );
            s.inputs[0] = s.control(
                "BUTTON",
                "入力スピーカーを使用する",
                BS_AUTOCHECKBOX as u32,
                345,
                20,
                132,
                350,
                28,
            )?;
            check(s.inputs[0], settings.input_enabled[0]);
            s.control(
                "BUTTON",
                "再読込",
                BS_PUSHBUTTON as u32,
                Command::Refresh as usize,
                700,
                132,
                112,
                28,
            )?;
            s.building_input.set(0);
            s.devices[0] = s.combo(301, 20, 164, 792)?;
            s.building_input.set(3);
            s.inputs[1] = s.control(
                "BUTTON",
                "入力アプリを使用する",
                BS_AUTOCHECKBOX as u32,
                346,
                20,
                198,
                350,
                28,
            )?;
            check(s.inputs[1], settings.input_enabled[1]);
            s.building_input.set(1);
            s.app = s.combo(APPLICATION_CONTROL, 20, 228, 792)?;
            s.app_detail = s.control(
                "EDIT",
                "",
                ES_READONLY as u32 | ES_AUTOHSCROLL as u32,
                303,
                20,
                262,
                792,
                25,
            )?;
            s.building_input.set(3);
            s.inputs[2] = s.control(
                "BUTTON",
                "入力ブラウザを使用する",
                BS_AUTOCHECKBOX as u32,
                347,
                20,
                294,
                350,
                28,
            )?;
            check(s.inputs[2], settings.input_enabled[2]);
            s.control(
                "BUTTON",
                "インストール案内",
                BS_OWNERDRAW as u32,
                BROWSER_HELP,
                410,
                294,
                180,
                28,
            )?;
            s.control(
                "BUTTON",
                "設定案内",
                BS_OWNERDRAW as u32,
                BROWSER_SETUP_HELP,
                610,
                294,
                202,
                28,
            )?;
            s.building_input.set(2);
            s.label("ポート", 20, 327, 76)?;
            s.port = s.edit(&settings.browser_port.to_string(), 330, 96, 324, 82)?;
            s.label("ストリームキー", 202, 327, 120)?;
            s.stream = s.edit(&settings.browser_stream_key, 331, 330, 324, 482)?;
            s.label("認証トークン", 20, 365, 112)?;
            s.token = s.edit(&settings.browser_token, 332, 140, 362, 672)?;
            s.building_input.set(3);
            for i in 0..2 {
                let y = 404 + i as i32 * 40;
                s.label(&format!("出力{}", i + 1), 20, y + 2, 52)?;
                s.outputs[i] = s.control(
                    "BUTTON",
                    "有効",
                    BS_AUTOCHECKBOX as u32,
                    310 + i,
                    744,
                    y,
                    68,
                    28,
                )?;
                check(s.outputs[i], settings.outputs[i].enabled);
                s.labels[i] = s.edit(&settings.output_labels[i], 340 + i, 80, y, 150)?;
                s.devices[i + 1] = s.combo(320 + i, 240, y, 492)?;
            }
            s.control(
                "BUTTON",
                "GitHub（公開リポジトリ）",
                BS_OWNERDRAW as u32,
                REPOSITORY_LINK,
                20,
                536,
                430,
                32,
            )?;
            s.building_page.set(1);
            s.build_hotkeys(settings)?;
            s.building_page.set(2);
            s.message = s.control("STATIC", warning, 0, 334, 20, 494, 792, 34)?;
            s.control(
                "BUTTON",
                "適用",
                BS_DEFPUSHBUTTON as u32,
                Command::Save as usize,
                514,
                CONTENT_OFFSET + 496,
                148,
                32,
            )?;
            s.control(
                "BUTTON",
                "閉じる",
                BS_PUSHBUTTON as u32,
                Command::Cancel as usize,
                676,
                CONTENT_OFFSET + 496,
                136,
                32,
            )?;
            s.refresh()?;
            s.input_selection_changed();
            s.set_page(0);
            Ok(s)
        }
    }
    fn build_hotkeys(&mut self, settings: &Settings) -> Result<(), String> {
        let choices = key_choices();
        self.label("設定", 20, 68, 180)?;
        self.label("キー", 580, 68, 100)?;
        self.label("モード", 696, 68, 116)?;
        self.control("STATIC", "", SEPARATOR_STYLE, 351, 20, 174, 792, 2)?;
        self.label("入力", 20, 188, 180)?;
        for (index, y) in [(5, 96), (4, 132), (0, 216), (1, 252), (2, 288), (3, 324)] {
            self.label(HOTKEY_LABELS[index], 20, y, 184)?;
            let key = &settings.hotkeys[index];
            let mut row = [HWND::default(); 7];
            // IDと保存配列は従来の意味を維持し、作成順と座標を表示順にする。
            for (column, label, enabled, x, width) in [
                (0, "有効", key.is_assigned(), 208, 60),
                (2, "Ctrl", key.ctrl, 274, 62),
                (1, "Shift", key.shift, 340, 70),
                (4, "Alt", key.alt, 416, 58),
                (3, "Windows", key.win, 480, 96),
            ] {
                row[column] = self.control(
                    "BUTTON",
                    label,
                    BS_AUTOCHECKBOX as u32,
                    HOTKEY_BASE + index * 10 + column,
                    x,
                    y,
                    width,
                    28,
                )?;
                check(row[column], enabled);
            }
            row[5] = self.combo(HOTKEY_BASE + index * 10 + 5, 580, y, 108)?;
            let label = if key.key == "None" {
                "なし"
            } else {
                &key.key
            };
            let selected = choices.iter().position(|value| value == label).unwrap_or(0);
            combo_items(row[5], choices.iter().cloned(), selected);
            row[6] = self.combo(HOTKEY_BASE + index * 10 + 6, 696, y, 116)?;
            combo_items(
                row[6],
                ["共存".into(), "上書き".into()],
                usize::from(key.mode == KeyMode::Override),
            );
            self.keys[index] = row;
        }
        self.control(
            "STATIC",
            "共存：同じキー入力を他のアプリにも渡し、SpkRouterでも操作します。",
            0,
            354,
            20,
            384,
            792,
            24,
        )?;
        self.control(
            "STATIC",
            "上書き：他のアプリへのキー入力を可能な範囲で抑止し、SpkRouterで操作します。",
            0,
            355,
            20,
            420,
            792,
            24,
        )?;
        Ok(())
    }
    pub(super) fn key_selection_changed(&self) {
        for (index, row) in self.keys.iter().enumerate() {
            let available = index >= 3 || checked(self.inputs[index]);
            let enabled = available && checked(row[0]) && !self.applying.get();
            unsafe {
                let _ = EnableWindow(row[0], available && !self.applying.get());
            }
            for hwnd in &row[1..] {
                unsafe {
                    let _ = EnableWindow(*hwnd, enabled);
                }
            }
        }
    }
    pub(super) fn input_selection_changed(&self) {
        for (index, controls) in self.input_controls.borrow().iter().enumerate() {
            for hwnd in controls {
                unsafe {
                    let _ =
                        EnableWindow(*hwnd, checked(self.inputs[index]) && !self.applying.get());
                }
            }
        }
        self.key_selection_changed();
    }
    pub(super) fn applied(&mut self, settings: &Settings) {
        self.base = settings.clone();
        self.overlay_edited = false;
        for (row, key) in self.keys.iter().zip(&settings.hotkeys) {
            check(row[0], key.enabled);
        }
        self.input_selection_changed();
        self.message("適用しました");
    }
    fn overlay_mode(&self) -> OverlayMode {
        match selected(self.overlay) {
            0 => OverlayMode::Off,
            1 => OverlayMode::Brief,
            _ => OverlayMode::Always,
        }
    }
    pub(super) fn sync_runtime(&mut self, settings: &Settings) {
        // 一度でも未適用の変更を認めた欄は、その後に実行値と一致しても保持する。
        self.overlay_edited |= self.overlay_mode() != self.base.overlay_mode;
        if !self.overlay_edited && settings.overlay_mode != self.base.overlay_mode {
            let index = match settings.overlay_mode {
                OverlayMode::Off => 0,
                OverlayMode::Brief => 1,
                OverlayMode::Always => 2,
            };
            unsafe {
                SendMessageW(self.overlay, CB_SETCURSEL, Some(WPARAM(index)), None);
            }
        }
        self.base.mode = settings.mode;
        self.base.overlay_mode = settings.overlay_mode;
    }
    fn control(
        &self,
        class: &str,
        value: &str,
        style: u32,
        id: usize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<HWND, String> {
        unsafe {
            let instance = self._class.instance();
            let tab = if class == "STATIC" {
                WINDOW_STYLE(0)
            } else {
                WS_TABSTOP
            };
            let hwnd = CreateWindowExW(
                if class == "EDIT" {
                    WS_EX_CLIENTEDGE
                } else {
                    WINDOW_EX_STYLE(0)
                },
                PCWSTR(wide(class).as_ptr()),
                PCWSTR(wide(value).as_ptr()),
                WS_CHILD | WS_VISIBLE | tab | WINDOW_STYLE(style),
                x,
                y,
                width,
                height,
                Some(self.hwnd),
                Some(HMENU(id as *mut _)),
                Some(instance.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            if self.building_page.get() < 2 {
                self.pages.borrow_mut()[self.building_page.get()].push(hwnd);
            }
            if self.building_input.get() < 3 {
                self.input_controls.borrow_mut()[self.building_input.get()].push(hwnd);
            }
            SendMessageW(
                hwnd,
                WM_SETFONT,
                Some(WPARAM(self.font.0 as usize)),
                Some(LPARAM(1)),
            );
            Ok(hwnd)
        }
    }
    fn label(&self, value: &str, x: i32, y: i32, width: i32) -> Result<(), String> {
        self.control("STATIC", value, 0, 0, x, y, width, 24)
            .map(|_| ())
    }
    fn combo(&self, id: usize, x: i32, y: i32, width: i32) -> Result<HWND, String> {
        self.control(
            "COMBOBOX",
            "",
            CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
            id,
            x,
            y,
            width,
            270,
        )
    }
    fn edit(&self, value: &str, id: usize, x: i32, y: i32, width: i32) -> Result<HWND, String> {
        self.control("EDIT", value, ES_AUTOHSCROLL as u32, id, x, y, width, 28)
    }
    pub(super) fn message(&self, value: &str) {
        set_text(self.message, value);
    }
    pub(super) fn app_path(&self) {
        let value = self
            .app_values
            .get(selected(self.app))
            .map(|v| v.0.to_string_lossy().into_owned())
            .unwrap_or_default();
        set_text(self.app_detail, &value);
    }
    pub(super) fn refresh(&mut self) -> Result<(), String> {
        // 候補更新だけで未保存の入力やチェック状態を変更しない。
        let current: [(String, String); 3] = std::array::from_fn(|i| {
            self.device_values[i]
                .get(selected(self.devices[i]))
                .cloned()
                .unwrap_or_else(|| {
                    if i == 0 {
                        (self.base.device_id.clone(), self.base.device_name.clone())
                    } else {
                        (
                            self.base.outputs[i - 1].endpoint_id.clone(),
                            self.base.outputs[i - 1].name.clone(),
                        )
                    }
                })
        });
        let app = self
            .app_values
            .get(selected(self.app))
            .map(|v| v.0.clone())
            .unwrap_or_else(|| self.base.application_path.clone());
        let mut choices: [Vec<(String, String)>; 3] = std::array::from_fn(|_| Vec::new());
        for i in 0..3 {
            choices[i] = device_choices(&current[i].0, &current[i].1)?;
        }
        for i in 0..3 {
            let index = choices[i]
                .iter()
                .position(|v| v.0 == current[i].0)
                .unwrap_or(0);
            combo_items(
                self.devices[i],
                choices[i].iter().map(|v| v.1.clone()),
                index,
            );
        }
        self.device_values = choices;
        self.app_values = application_choices(&app);
        let index = self.app_values.iter().position(|v| v.0 == app).unwrap_or(0);
        combo_items(self.app, self.app_values.iter().map(|v| v.1.clone()), index);
        self.app_path();
        Ok(())
    }
    pub(super) fn read(&self) -> Result<Settings, String> {
        let mut settings = self.base.clone();
        settings.input_enabled = self.inputs.map(checked);
        for (index, row) in self.keys.iter().enumerate() {
            let key = text(row[5]);
            settings.hotkeys[index] = Hotkey {
                enabled: checked(row[0]) && key != "なし",
                shift: checked(row[1]),
                ctrl: checked(row[2]),
                win: checked(row[3]),
                alt: checked(row[4]),
                key: if key == "なし" { "None".into() } else { key },
                mode: if selected(row[6]) == 1 {
                    KeyMode::Override
                } else {
                    KeyMode::Coexist
                },
            };
        }
        for i in 0..3 {
            let (id, name) = self.device_values[i]
                .get(selected(self.devices[i]))
                .cloned()
                .ok_or("再生デバイスを選択してください")?;
            // 未接続の表示注記を設定名へ積み重ねない。
            let name = name.trim_end_matches("（未接続）").to_owned();
            if i == 0 {
                settings.device_id = id;
                settings.device_name = name;
            } else {
                settings.outputs[i - 1].endpoint_id = id;
                settings.outputs[i - 1].name = name;
                settings.outputs[i - 1].enabled = checked(self.outputs[i - 1]);
            }
        }
        settings.application_path = self
            .app_values
            .get(selected(self.app))
            .map(|v| v.0.clone())
            .ok_or("アプリを選択してください")?;
        if settings.input_enabled[2] {
            settings.browser_port = text(self.port)
                .trim()
                .parse()
                .map_err(|_| "ポートは1〜65535で指定してください")?;
            settings.browser_token = text(self.token).trim().to_owned();
            settings.browser_stream_key = text(self.stream).trim().to_owned();
        }
        settings.overlay_mode = self.overlay_mode();
        settings.startup_mode = match selected(self.startup) {
            1 => Some(Mode::Device),
            2 => Some(Mode::Application),
            3 => Some(Mode::Browser),
            4 => Some(Mode::Stopped),
            _ => None,
        };
        settings.output_labels = self.labels.map(|label| text(label).trim().to_owned());
        settings.validate()?;
        Ok(settings)
    }
}
impl Drop for SettingsWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = DeleteObject(self.font.into());
        }
    }
}

impl SettingsWindow {
    pub(super) fn show(&self) {
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            if !IsWindowVisible(self.hwnd).as_bool() {
                // 起動スクリプトのSW_HIDE指定でも、明示的な設定表示は実行する。
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            if GetForegroundWindow() != self.hwnd {
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
    }
    pub(super) fn dialog_message(&self, msg: &MSG) -> bool {
        unsafe {
            if msg.message == WM_KEYDOWN
                && msg.wParam.0 == VK_TAB.0 as usize
                && GetKeyState(VK_CONTROL.0 as i32) < 0
                && (msg.hwnd == self.hwnd || IsChild(self.hwnd, msg.hwnd).as_bool())
            {
                if !self.applying.get() {
                    let next = 1 - SendMessageW(self.tab, TCM_GETCURSEL, None, None)
                        .0
                        .clamp(0, 1);
                    queue(if next == 0 {
                        Command::BasicPage
                    } else {
                        Command::KeyPage
                    });
                    let _ = SetFocus(Some(self.tab));
                }
                return true;
            }
            (WM_KEYFIRST..=WM_KEYLAST).contains(&msg.message)
                && (msg.hwnd == self.hwnd || IsChild(self.hwnd, msg.hwnd).as_bool())
                && IsDialogMessageW(self.hwnd, msg).as_bool()
        }
    }
    pub(super) fn set_page(&self, page: usize) {
        unsafe {
            SendMessageW(self.tab, TCM_SETCURSEL, Some(WPARAM(page)), None);
        }
        for (index, controls) in self.pages.borrow().iter().enumerate() {
            for hwnd in controls {
                unsafe {
                    let _ = ShowWindow(*hwnd, if index == page { SW_SHOW } else { SW_HIDE });
                }
            }
        }
    }
    pub(super) fn set_applying(&self, applying: bool) {
        self.applying.set(applying);
        unsafe {
            let mut child = GetWindow(self.hwnd, GW_CHILD);
            while let Ok(hwnd) = child {
                if hwnd != self.message {
                    let _ =
                        windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(hwnd, !applying);
                }
                child = GetWindow(hwnd, GW_HWNDNEXT);
            }
        }
        self.input_selection_changed();
    }
    pub(super) fn hwnd(&self) -> HWND {
        self.hwnd
    }
    #[cfg(test)]
    pub(super) fn edit_for_test(&self, stream: &str, overlay: bool) {
        set_text(self.stream, stream);
        unsafe {
            SendMessageW(
                self.overlay,
                CB_SETCURSEL,
                Some(WPARAM(if overlay { 2 } else { 0 })),
                None,
            );
        }
    }
    #[cfg(test)]
    pub(super) fn caption(&self) -> String {
        text(self.hwnd)
    }
    #[cfg(test)]
    pub(super) fn set_hotkey_for_test(&self, index: usize, binding: &Hotkey) {
        let row = &self.keys[index];
        for (handle, value) in row[..5].iter().zip([
            binding.enabled,
            binding.shift,
            binding.ctrl,
            binding.win,
            binding.alt,
        ]) {
            check(*handle, value);
        }
        let key = if binding.key == "None" {
            "なし"
        } else {
            &binding.key
        };
        let index = key_choices().iter().position(|value| value == key).unwrap();
        unsafe {
            SendMessageW(row[5], CB_SETCURSEL, Some(WPARAM(index)), None);
            SendMessageW(
                row[6],
                CB_SETCURSEL,
                Some(WPARAM(usize::from(binding.mode == KeyMode::Override))),
                None,
            );
        }
        self.key_selection_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::{checked, SettingsWindow, HOTKEY_BASE, SETTINGS_HEIGHT, SETTINGS_WIDTH};
    use crate::{
        config::Settings,
        key_binding::{key_choices, Hotkey, HOTKEY_COUNT},
        ui::commands::{clear, drain, Command},
    };
    use windows::Win32::{
        Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
        Graphics::Gdi::ClientToScreen,
        UI::WindowsAndMessaging::*,
    };

    fn select_key(window: &SettingsWindow, row: usize, key: &str) {
        let index = key_choices().iter().position(|value| value == key).unwrap();
        clear();
        unsafe {
            SendMessageW(window.keys[row][5], CB_SETCURSEL, Some(WPARAM(index)), None);
            SendMessageW(
                window.hwnd,
                WM_COMMAND,
                Some(WPARAM(
                    (CBN_SELCHANGE as usize) << 16 | (HOTKEY_BASE + row * 10 + 5),
                )),
                Some(LPARAM(window.keys[row][5].0 as isize)),
            );
        }
        assert_eq!(drain(), [(Command::KeyChanged, false)]);
        window.key_selection_changed();
    }

    fn client_rect(window: HWND, child: HWND) -> RECT {
        unsafe {
            let mut origin = POINT::default();
            assert!(ClientToScreen(window, &mut origin).as_bool());
            let mut rect = RECT::default();
            GetWindowRect(child, &mut rect).unwrap();
            rect.left -= origin.x;
            rect.right -= origin.x;
            rect.top -= origin.y;
            rect.bottom -= origin.y;
            rect
        }
    }

    fn assert_visible_layout(window: &SettingsWindow) {
        unsafe {
            let mut bounds = RECT::default();
            GetClientRect(window.hwnd, &mut bounds).unwrap();
            assert_eq!(
                (bounds.right, bounds.bottom),
                (SETTINGS_WIDTH, SETTINGS_HEIGHT)
            );
            for page in 0..2 {
                window.set_page(page);
                let mut rectangles = Vec::new();
                let mut child = GetWindow(window.hwnd, GW_CHILD);
                while let Ok(hwnd) = child {
                    if IsWindowVisible(hwnd).as_bool() && hwnd != window.tab {
                        let rect = client_rect(window.hwnd, hwnd);
                        assert!(
                            rect.left >= 0
                                && rect.top >= 0
                                && rect.right <= bounds.right
                                && rect.bottom <= bounds.bottom
                                && rect.right > rect.left
                                && rect.bottom > rect.top,
                            "画面外: {rect:?}"
                        );
                        rectangles.push(rect);
                    }
                    child = GetWindow(hwnd, GW_HWNDNEXT);
                }
                for (i, a) in rectangles.iter().enumerate() {
                    for b in &rectangles[i + 1..] {
                        assert!(
                            a.right <= b.left
                                || b.right <= a.left
                                || a.bottom <= b.top
                                || b.bottom <= a.top,
                            "重なり: {a:?}, {b:?}"
                        );
                    }
                }
                for (index, controls) in window.pages.borrow().iter().enumerate() {
                    for hwnd in controls {
                        assert_eq!(IsWindowVisible(*hwnd).as_bool(), index == page);
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "Windows の設定窓でキー編集と実コントロールの配置を確認する実機 GUI 試験"]
    fn hotkey_rows_roundtrip_preserve_drafts_and_fit_client_area() {
        let settings = Settings::default();
        let mut window = SettingsWindow::new(&settings, HICON::default(), "").unwrap();
        unsafe {
            use windows::Win32::UI::Controls::TCM_GETITEMCOUNT;
            assert_eq!(SendMessageW(window.tab, TCM_GETITEMCOUNT, None, None).0, 2);
            for combo in
                window
                    .devices
                    .into_iter()
                    .chain([window.app, window.overlay, window.startup])
            {
                let rect = client_rect(window.hwnd, combo);
                assert_eq!(
                    SendMessageW(combo, CB_GETDROPPEDWIDTH, None, None).0,
                    (rect.right - rect.left) as isize,
                    "展開幅は閉じた欄と同じ幅"
                );
            }
        }
        let basic_order = [
            window.overlay,
            window.startup,
            window.devices[0],
            window.app,
            window.port,
            window.outputs[0],
            window.outputs[1],
        ];
        for controls in basic_order.windows(2) {
            assert!(
                client_rect(window.hwnd, controls[0]).bottom
                    <= client_rect(window.hwnd, controls[1]).top
            );
        }
        assert_eq!(window.read().unwrap().hotkeys, settings.hotkeys);
        let ordered: Vec<_> = [5, 4, 0, 1, 2, 3]
            .into_iter()
            .map(|i| client_rect(window.hwnd, window.keys[i][0]))
            .collect();
        for pair in ordered.windows(2) {
            assert!(pair[0].bottom < pair[1].top);
        }
        for row in &window.keys {
            for pair in [row[0], row[2], row[1], row[4], row[3], row[5], row[6]].windows(2) {
                assert!(
                    client_rect(window.hwnd, pair[0]).right
                        <= client_rect(window.hwnd, pair[1]).left
                );
            }
        }
        unsafe {
            for (id, expected) in [
                (super::BROWSER_HELP, Command::BrowserInstallHelp),
                (super::BROWSER_SETUP_HELP, Command::BrowserSetupHelp),
                (super::REPOSITORY_LINK, Command::OpenRepository),
            ] {
                clear();
                let link = GetDlgItem(Some(window.hwnd), id as i32).unwrap();
                SendMessageW(link, BM_CLICK, None, None);
                assert_eq!(drain(), [(expected, false)]);
            }
        }
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
            super::check(window.inputs[0], false);
            window.input_selection_changed();
            assert!(!IsWindowEnabled(window.devices[0]).as_bool());
            assert!(!IsWindowEnabled(window.keys[0][0]).as_bool());
            assert!(!window.read().unwrap().input_enabled[0]);
            assert_eq!(window.read().unwrap().hotkeys, settings.hotkeys);
            window.set_applying(true);
            window.set_applying(false);
            assert!(!IsWindowEnabled(window.devices[0]).as_bool());
            super::check(window.inputs[0], true);
            window.input_selection_changed();
            assert!(IsWindowEnabled(window.devices[0]).as_bool());
            assert!(IsWindowEnabled(window.keys[0][0]).as_bool());
        }
        let mut draft: [Hotkey; HOTKEY_COUNT] = std::array::from_fn(|index| Hotkey {
            enabled: index != 1,
            shift: index == 0,
            ctrl: index == 1,
            win: index == 2,
            alt: index == 3,
            key: ["A", "F24", "Z", "F5", "F6", "F7"][index].into(),
            mode: if index % 2 == 0 {
                crate::key_binding::KeyMode::Override
            } else {
                crate::key_binding::KeyMode::Coexist
            },
        });
        for (index, binding) in draft.iter().enumerate() {
            window.set_hotkey_for_test(index, binding);
        }
        assert_eq!(window.read().unwrap().hotkeys, draft);
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
            assert!(IsWindowEnabled(window.keys[1][0]).as_bool());
            for hwnd in &window.keys[1][1..] {
                assert!(!IsWindowEnabled(*hwnd).as_bool());
            }
            window.set_applying(true);
            assert!(!IsWindowEnabled(window.keys[0][0]).as_bool());
            window.set_applying(false);
            assert!(IsWindowEnabled(window.keys[0][5]).as_bool());
            assert!(!IsWindowEnabled(window.keys[1][5]).as_bool());
            // 未割当の行も「有効」を先にチェックすればキーを選べる。
            super::check(window.keys[4][0], false);
            window.key_selection_changed();
            assert!(!IsWindowEnabled(window.keys[4][5]).as_bool());
            super::check(window.keys[4][0], true);
            window.key_selection_changed();
            assert!(IsWindowEnabled(window.keys[4][5]).as_bool());
        }
        window.message("検証中");
        window.refresh().unwrap();
        assert_eq!(window.read().unwrap().hotkeys, draft);

        select_key(&window, 0, "なし");
        assert!(!checked(window.keys[0][0]));
        draft[0].enabled = false;
        draft[0].key = "None".into();
        assert_eq!(window.read().unwrap().hotkeys, draft);
        select_key(&window, 0, "B");
        assert!(
            !checked(window.keys[0][0]),
            "別のキーを選んでも有効化しない"
        );
        draft[0].key = "B".into();
        assert_eq!(window.read().unwrap().hotkeys, draft);

        draft[0].enabled = true;
        window.set_hotkey_for_test(0, &draft[0]);
        window.set_hotkey_for_test(1, &draft[0]);
        assert!(window.read().unwrap_err().contains("同じキー"));
        draft[1] = draft[0].clone();
        draft[1].enabled = false;
        window.set_hotkey_for_test(1, &draft[1]);
        assert_eq!(window.read().unwrap().hotkeys, draft);
        window.refresh().unwrap();
        assert_eq!(window.read().unwrap().hotkeys, draft);

        window.set_page(0);
        window.show();
        unsafe {
            use windows::Win32::UI::Controls::{EM_GETSEL, EM_SETSEL};
            use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
            // SetFocusは以前のフォーカスがない場合、成功してもNULLを返す。
            let _ = SetFocus(Some(window.labels[0]));
            assert_eq!(GetFocus(), window.labels[0]);
            SendMessageW(
                window.labels[0],
                EM_SETSEL,
                Some(WPARAM(1)),
                Some(LPARAM(3)),
            );
            let focus = GetFocus();
            let selection = SendMessageW(window.labels[0], EM_GETSEL, None, None);
            for _ in 0..5 {
                window.show();
            }
            assert_eq!(GetFocus(), focus);
            assert_eq!(
                SendMessageW(window.labels[0], EM_GETSEL, None, None),
                selection
            );
            assert!(!window.dialog_message(&MSG {
                hwnd: window.hwnd,
                message: WM_TIMER,
                ..Default::default()
            }));
            assert!(!window.dialog_message(&MSG {
                message: WM_KEYDOWN,
                ..Default::default()
            }));
        }
        assert_visible_layout(&window);
        let saved = window.read().unwrap();
        drop(window);
        let reopened = SettingsWindow::new(&saved, HICON::default(), "").unwrap();
        assert_eq!(reopened.read().unwrap().hotkeys, draft);
    }
}
