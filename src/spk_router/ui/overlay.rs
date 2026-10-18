//! 常駐表示。状態変化・表示期限・メーターの更新を分け、フォーカスを奪わない。
use super::{
    commands::{queue, Command},
    native::{font, title, wide, WindowClass},
};
use crate::{
    config::{Mode, OverlayMode, Settings},
    key_binding::HOTKEY_COUNT,
    state::Phase,
    telemetry::{level_height, Snapshot, BINS},
};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicU8, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};
const OVERLAY_CLASS: &str = "VoiceMixerSpkOverlayV1";
const OVERLAY_WIDTH: i32 = 464;
const NORMAL_HEIGHT: i32 = 130;
const ERROR_HEIGHT: i32 = 156;
const INITIAL_MARGIN: i32 = 80;
// Mic版（上80px・高さ130px）の下へ20px空けて並べる。
const INITIAL_TOP: i32 = INITIAL_MARGIN + 130 + 20;
const BRIEF: Duration = Duration::from_secs(4);
const INPUT_METER_RECT: RECT = RECT {
    left: 60,
    top: 61,
    right: OVERLAY_WIDTH - 12,
    bottom: 77,
};
const OUTPUT_METER_RECT: RECT = RECT {
    top: 108,
    bottom: 124,
    ..INPUT_METER_RECT
};
#[path = "overlay/drawing.rs"]
mod drawing;
use drawing::draw_overlay;
static DRAG: Mutex<Option<(POINT, RECT, bool, Option<Command>)>> = Mutex::new(None);
static PAINT: Mutex<Option<DisplayState>> = Mutex::new(None);
#[derive(Clone, Copy, Default, PartialEq)]
struct MeterPaint {
    bins: [[Option<f32>; BINS]; 3],
    active: [bool; 3],
    fresh: bool,
}
static LEVELS: Mutex<MeterPaint> = Mutex::new(MeterPaint {
    bins: [[None; BINS]; 3],
    active: [false; 3],
    fresh: false,
});
static REMAINING: AtomicU8 = AtomicU8::new(0);
#[derive(Clone, PartialEq, Eq)]
pub(super) struct DisplayState {
    mode: Mode,
    inputs: [bool; 3],
    phase: Phase,
    outputs: [bool; 2],
    keys: [String; HOTKEY_COUNT],
    labels: [String; 2],
    overlay: OverlayMode,
    title: String,
    message: String,
    editing: bool,
    failed: bool,
}
const CONTROLS: [Command; 8] = [
    Command::ToggleOverlay,
    Command::Show,
    Command::Device,
    Command::Application,
    Command::Browser,
    Command::Stopped,
    Command::Output1,
    Command::Output2,
];
fn control_rect(command: Command, inputs: [bool; 3]) -> Option<RECT> {
    let (top, index, count) = match command {
        Command::ToggleOverlay => (8, 0, 2),
        Command::Show => (8, 1, 2),
        Command::Device | Command::Application | Command::Browser | Command::Stopped => {
            let visible: Vec<_> = [
                Command::Device,
                Command::Application,
                Command::Browser,
                Command::Stopped,
            ]
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i == 3 || inputs[*i])
            .map(|(_, c)| c)
            .collect();
            (
                35,
                visible.iter().position(|c| *c == command)? as i32,
                visible.len() as i32,
            )
        }
        Command::Output1 => (82, 0, 2),
        Command::Output2 => (82, 1, 2),
        _ => return None,
    };
    let width = (OVERLAY_WIDTH - 72 - (count - 1) * 4) / count;
    let left = 60 + index * (width + 4);
    Some(RECT {
        left,
        top,
        right: left + width,
        bottom: top + 23,
    })
}
fn hit_control(point: POINT) -> Option<Command> {
    let inputs = PAINT.lock().unwrap().as_ref()?.inputs;
    hit_control_with(point, inputs)
}
fn hit_control_with(point: POINT, inputs: [bool; 3]) -> Option<Command> {
    CONTROLS.into_iter().find(|c| {
        control_rect(*c, inputs).is_some_and(|r| {
            point.x >= r.left && point.x < r.right && point.y >= r.top && point.y < r.bottom
        })
    })
}
fn mouse_point(value: LPARAM) -> POINT {
    POINT {
        x: value.0 as i16 as i32,
        y: (value.0 >> 16) as i16 as i32,
    }
}
fn moved(a: POINT, b: POINT) -> bool {
    (a.x - b.x).abs() >= 4 || (a.y - b.y).abs() >= 4
}
fn allowed(command: Command) -> bool {
    PAINT
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|s| !s.editing || command == Command::Show)
}
struct VisibilityClock {
    mode: OverlayMode,
    deadline: Option<Instant>,
}
impl VisibilityClock {
    fn new() -> Self {
        Self {
            mode: OverlayMode::Off,
            deadline: None,
        }
    }
    fn update(
        &mut self,
        mode: OverlayMode,
        changed: bool,
        dragging: bool,
        now: Instant,
    ) -> (bool, u8) {
        if mode != self.mode || changed || dragging {
            self.deadline = if mode == OverlayMode::Brief {
                Some(now + BRIEF)
            } else {
                None
            };
        }
        self.mode = mode;
        match mode {
            OverlayMode::Off => (false, 0),
            OverlayMode::Always => (true, 0),
            OverlayMode::Brief => {
                let left = self
                    .deadline
                    .map(|d| d.saturating_duration_since(now))
                    .unwrap_or_default();
                (!left.is_zero(), ((left.as_millis() + 999) / 1000) as u8)
            }
        }
    }
}
unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
            WM_LBUTTONDBLCLK => {
                queue(Command::Show);
                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                queue(Command::Menu);
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                let mut point = POINT::default();
                let _ = GetCursorPos(&mut point);
                let _ = ScreenToClient(hwnd, &mut point);
                let dragging = DRAG.lock().unwrap().as_ref().is_some_and(|d| d.2);
                let cursor = if dragging {
                    IDC_SIZEALL
                } else if hit_control(point).is_some_and(allowed) {
                    IDC_HAND
                } else if PAINT.lock().unwrap().as_ref().is_some_and(|s| s.editing) {
                    IDC_ARROW
                } else {
                    IDC_SIZEALL
                };
                if let Ok(cursor) = LoadCursorW(None, cursor) {
                    SetCursor(Some(cursor));
                }
                return LRESULT(1);
            }
            WM_LBUTTONDOWN => {
                let point = mouse_point(l);
                let mut screen = point;
                let mut rect = RECT::default();
                if ClientToScreen(hwnd, &mut screen).as_bool()
                    && GetWindowRect(hwnd, &mut rect).is_ok()
                {
                    *DRAG.lock().unwrap() = Some((screen, rect, false, hit_control(point)));
                    SetCapture(hwnd);
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let mut point = POINT::default();
                if GetCursorPos(&mut point).is_ok() {
                    let position = {
                        let mut drag = DRAG.lock().unwrap();
                        drag.as_mut().and_then(|(start, rect, dragged, _)| {
                            *dragged |= moved(*start, point);
                            dragged.then_some((
                                rect.left + point.x - start.x,
                                rect.top + point.y - start.y,
                            ))
                        })
                    };
                    if let Some((left, top)) = position {
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            left,
                            top,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                        );
                    }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let drag = DRAG.lock().unwrap().take();
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                if let Some((start, _, false, Some(command))) = drag {
                    let point = mouse_point(l);
                    let mut screen = point;
                    if ClientToScreen(hwnd, &mut screen).as_bool()
                        && !moved(start, screen)
                        && hit_control(point) == Some(command)
                        && allowed(command)
                    {
                        queue(command);
                    }
                }
                return LRESULT(0);
            }
            WM_CAPTURECHANGED | WM_CANCELMODE | WM_DESTROY => {
                *DRAG.lock().unwrap() = None;
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let target = BeginPaint(hwnd, &mut paint);
                let mut bounds = RECT::default();
                let _ = GetClientRect(hwnd, &mut bounds);
                let state = PAINT.lock().unwrap().clone();
                if let Some(state) = state {
                    if let Some(buffer) = Buffer::new(target, bounds.right, bounds.bottom) {
                        draw_overlay(buffer.dc, bounds, &state);
                        let r = paint.rcPaint;
                        let _ = BitBlt(
                            target,
                            r.left,
                            r.top,
                            r.right - r.left,
                            r.bottom - r.top,
                            Some(buffer.dc),
                            r.left,
                            r.top,
                            SRCCOPY,
                        );
                    } else {
                        draw_overlay(target, bounds, &state);
                    }
                }
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}
struct Buffer {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
}
impl Buffer {
    unsafe fn new(target: HDC, width: i32, height: i32) -> Option<Self> {
        unsafe {
            let dc = CreateCompatibleDC(Some(target));
            if dc.0.is_null() {
                return None;
            }
            let bitmap = CreateCompatibleBitmap(target, width, height);
            if bitmap.0.is_null() {
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap.into());
            if old.0.is_null() || old.0 as isize == -1 {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return None;
            }
            Some(Self { dc, bitmap, old })
        }
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
unsafe fn fill(dc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(dc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}
unsafe fn draw_text(dc: HDC, value: &str, mut rect: RECT, flags: DRAW_TEXT_FORMAT, color: u32) {
    // 空文字のVecは実体を持たないポインターになる。Win32へ渡さず描画を省く。
    if value.is_empty() {
        return;
    }
    unsafe {
        SetTextColor(dc, COLORREF(color));
        DrawTextW(
            dc,
            &mut value.encode_utf16().collect::<Vec<_>>(),
            &mut rect,
            flags | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
}

fn key_suffix(key: &str) -> String {
    if key == "なし" {
        String::new()
    } else {
        format!("[{key}]")
    }
}
impl DisplayState {
    pub(super) fn new(
        settings: &Settings,
        phase: Phase,
        message: &str,
        warning: &str,
        editing: bool,
    ) -> Self {
        Self {
            mode: settings.mode,
            inputs: settings.input_enabled,
            phase,
            outputs: settings.outputs.each_ref().map(|o| o.enabled),
            keys: settings.hotkeys.each_ref().map(|k| k.key_label().into()),
            labels: settings.output_labels.clone(),
            overlay: settings.overlay_mode,
            title: if settings.mode == Mode::Stopped {
                "停止".into()
            } else {
                format!("{} {}", settings.mode.label(), phase.label())
            },
            message: if warning.is_empty() {
                message.into()
            } else {
                warning.into()
            },
            editing,
            failed: phase == Phase::Error || !warning.is_empty(),
        }
    }
    fn control_label(&self, command: Command) -> (String, bool) {
        let input = match command {
            Command::Device => Some((0, "デバイス", Mode::Device)),
            Command::Application => Some((1, "アプリ", Mode::Application)),
            Command::Browser => Some((2, "ブラウザ", Mode::Browser)),
            Command::Stopped => Some((3, "停止", Mode::Stopped)),
            _ => None,
        };
        if let Some((index, label, mode)) = input {
            return (
                format!("{label}{}", key_suffix(&self.keys[index])),
                self.mode == mode,
            );
        }
        match command {
            Command::Output1 | Command::Output2 => {
                let i = command as usize - Command::Output1 as usize;
                (
                    format!(
                        "{} {}",
                        self.labels[i],
                        if self.outputs[i] { "ON" } else { "OFF" }
                    ),
                    self.outputs[i],
                )
            }
            Command::Show => (format!("設定{}", key_suffix(&self.keys[4])), self.editing),
            Command::ToggleOverlay => {
                let seconds = REMAINING.load(Ordering::Relaxed);
                (
                    format!(
                        "表示{} {}{}",
                        key_suffix(&self.keys[5]),
                        self.overlay.label(),
                        if seconds > 0 {
                            format!(" {seconds}秒")
                        } else {
                            String::new()
                        }
                    ),
                    self.overlay != OverlayMode::Off,
                )
            }
            _ => (String::new(), false),
        }
    }
    pub(super) fn tooltip(&self) -> String {
        format!(
            "{} / {} {} / {} {} / 表示 {}{}",
            self.title,
            self.labels[0],
            if self.outputs[0] { "ON" } else { "OFF" },
            self.labels[1],
            if self.outputs[1] { "ON" } else { "OFF" },
            self.overlay.label(),
            if self.failed {
                format!(" / {}", self.message)
            } else {
                String::new()
            }
        )
    }
}
fn fit_output(dc: HDC, label: &str, suffix: &str, width: i32) -> String {
    let mut chars: Vec<char> = label.chars().collect();
    let mut shortened = false;
    loop {
        let text = format!(
            "{}{} {suffix}",
            chars.iter().collect::<String>(),
            if shortened { "…" } else { "" }
        );
        let mut size = windows::Win32::Foundation::SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(dc, &text.encode_utf16().collect::<Vec<_>>(), &mut size);
        }
        if size.cx <= width || chars.is_empty() {
            return text;
        }
        chars.pop();
        shortened = true;
    }
}
pub(super) struct OverlayWindow {
    hwnd: HWND,
    clock: RefCell<VisibilityClock>,
    _class: WindowClass,
}
impl OverlayWindow {
    pub(super) fn new() -> Result<Self, String> {
        *PAINT.lock().unwrap() = None;
        *DRAG.lock().unwrap() = None;
        *LEVELS.lock().unwrap() = MeterPaint::default();
        REMAINING.store(0, Ordering::Relaxed);
        let class = WindowClass::register(OVERLAY_CLASS, Some(overlay_proc), HBRUSH::default())?;
        unsafe {
            let mut work = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&mut work as *mut RECT).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(wide(OVERLAY_CLASS).as_ptr()),
                PCWSTR(wide(&title()).as_ptr()),
                WS_POPUP,
                (work.right - OVERLAY_WIDTH - INITIAL_MARGIN).max(work.left),
                work.top + INITIAL_TOP,
                OVERLAY_WIDTH,
                NORMAL_HEIGHT,
                None,
                None,
                Some(class.instance()),
                None,
            )
            .map_err(|e| e.to_string())?;
            Ok(Self {
                hwnd,
                clock: RefCell::new(VisibilityClock::new()),
                _class: class,
            })
        }
    }
    pub(super) fn update(&self, state: &DisplayState) -> bool {
        let changed = {
            let mut current = PAINT.lock().unwrap();
            if current.as_ref() == Some(state) {
                false
            } else {
                *current = Some(state.clone());
                true
            }
        };
        let dragging = DRAG.lock().unwrap().as_ref().is_some_and(|d| d.2);
        let (visible, seconds) =
            self.clock
                .borrow_mut()
                .update(state.overlay, changed, dragging, Instant::now());
        let previous = REMAINING.swap(seconds, Ordering::Relaxed);
        unsafe {
            if changed {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    OVERLAY_WIDTH,
                    if state.failed {
                        ERROR_HEIGHT
                    } else {
                        NORMAL_HEIGHT
                    },
                    SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            } else if previous != seconds {
                let _ = InvalidateRect(
                    Some(self.hwnd),
                    control_rect(Command::ToggleOverlay, state.inputs)
                        .as_ref()
                        .map(|r| r as *const RECT),
                    false,
                );
            }
            if IsWindowVisible(self.hwnd).as_bool() != visible {
                let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
        changed
    }
    pub(super) fn update_meters(&self, snapshot: Option<&Snapshot>, mode: Mode) {
        let now = Instant::now();
        let levels = snapshot
            .filter(|s| s.fresh(now, 0, mode))
            .map(|s| MeterPaint {
                bins: s.bins(now),
                active: s.active,
                fresh: true,
            })
            .unwrap_or_default();
        let changed = {
            let mut old = LEVELS.lock().unwrap();
            if *old == levels {
                false
            } else {
                *old = levels;
                true
            }
        };
        if changed {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), Some(&INPUT_METER_RECT), false);
                let _ = InvalidateRect(Some(self.hwnd), Some(&OUTPUT_METER_RECT), false);
            }
        }
    }
    #[cfg(test)]
    pub(super) fn painted_title(&self) -> String {
        PAINT.lock().unwrap().as_ref().unwrap().title.clone()
    }
}
impl Drop for OverlayWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        *PAINT.lock().unwrap() = None;
        *DRAG.lock().unwrap() = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn button_regions_are_equal_do_not_overlap_and_have_noninteractive_gaps() {
        for (i, a) in CONTROLS.into_iter().enumerate() {
            let a = control_rect(a, [true; 3]).unwrap();
            assert_eq!(a.bottom - a.top, 23);
            assert!(a.right <= OVERLAY_WIDTH - 12);
            for b in &CONTROLS[i + 1..] {
                let b = control_rect(*b, [true; 3]).unwrap();
                assert!(
                    a.right <= b.left
                        || b.right <= a.left
                        || a.bottom <= b.top
                        || b.bottom <= a.top
                );
            }
        }
        assert!(hit_control(POINT { x: 20, y: 60 }).is_none());
        let first = control_rect(Command::Device, [true; 3]).unwrap();
        assert!(hit_control_with(
            POINT {
                x: first.right + 1,
                y: 40
            },
            [true; 3]
        )
        .is_none());
        assert!(moved(POINT { x: 0, y: 0 }, POINT { x: 4, y: 0 }));
        assert!(!moved(POINT { x: 0, y: 0 }, POINT { x: 3, y: 3 }));
    }
    #[test]
    fn brief_deadline_countdown_and_off_do_not_depend_on_meter_updates() {
        let now = Instant::now();
        let mut c = VisibilityClock::new();
        assert_eq!(c.update(OverlayMode::Brief, true, false, now), (true, 4));
        assert_eq!(
            c.update(
                OverlayMode::Brief,
                false,
                false,
                now + Duration::from_millis(3100)
            ),
            (true, 1)
        );
        assert_eq!(
            c.update(OverlayMode::Brief, false, false, now + BRIEF),
            (false, 0)
        );
        assert_eq!(
            c.update(OverlayMode::Brief, true, false, now + BRIEF),
            (true, 4)
        );
        assert_eq!(
            c.update(OverlayMode::Off, true, true, now + BRIEF),
            (false, 0)
        );
        assert_eq!(
            c.update(OverlayMode::Always, true, false, now + BRIEF),
            (true, 0)
        );
    }
    #[test]
    fn hidden_inputs_leave_equal_width_buttons_and_no_hidden_hit_regions() {
        for mask in 0..8 {
            let enabled = std::array::from_fn(|i| mask & (1 << i) != 0);
            let mut previous: Option<RECT> = None;
            for (index, command) in [
                Command::Device,
                Command::Application,
                Command::Browser,
                Command::Stopped,
            ]
            .into_iter()
            .enumerate()
            {
                let rect = control_rect(command, enabled);
                assert_eq!(rect.is_some(), index == 3 || enabled[index]);
                if let Some(rect) = rect {
                    assert_eq!(
                        hit_control_with(
                            POINT {
                                x: rect.left + 2,
                                y: rect.top + 2
                            },
                            enabled
                        ),
                        Some(command)
                    );
                    assert!(rect.bottom < INPUT_METER_RECT.top);
                    if let Some(prev) = previous {
                        assert_eq!(prev.right - prev.left, rect.right - rect.left);
                        assert_eq!(prev.right + 4, rect.left);
                    }
                    previous = Some(rect);
                }
            }
            assert_eq!(previous.unwrap().right, OVERLAY_WIDTH - 12);
        }
    }
    #[test]
    fn no_assignment_is_hidden_and_output_names_do_not_add_output_keys() {
        let mut s = Settings::default();
        s.hotkeys[1].enabled = false;
        s.output_labels = ["配信用".into(), "音声入力用".into()];
        let state = DisplayState::new(&s, Phase::Stopped, "", "", false);
        assert_eq!(state.control_label(Command::Application).0, "アプリ");
        assert_eq!(state.control_label(Command::Device).0, "デバイス[F1]");
        assert_eq!(state.control_label(Command::Output1).0, "配信用 OFF");
        assert_eq!(state.control_label(Command::Show).0, "設定");
    }
}

#[cfg(test)]
mod width_tests {
    use super::*;
    #[test]
    fn five_fullwidth_characters_and_status_fit_without_shortening() {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let font = font(12);
            let previous = SelectObject(dc, font.into());
            let width = control_rect(Command::Output1, [true; 3]).unwrap().right
                - control_rect(Command::Output1, [true; 3]).unwrap().left
                - 8;
            assert_eq!(fit_output(dc, "音声入力用", "OFF", width), "音声入力用 OFF");
            let long = fit_output(dc, &"長い用途名".repeat(5), "OFF", width);
            assert!(long.ends_with(" OFF") && long.contains('…'));
            SelectObject(dc, previous);
            let _ = DeleteObject(font.into());
            let _ = DeleteDC(dc);
        }
    }
}
