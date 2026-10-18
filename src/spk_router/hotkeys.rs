//! 専用フックで押下と解放を観測し、上書き指定の一致キーだけを抑止する。
use crate::key_binding::{Hotkey, KeyMode};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{
                GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};
static EDITING: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicUsize = AtomicUsize::new(1);
pub(super) fn set_editing(editing: bool) {
    EDITING.store(editing, Ordering::Release);
}
pub(super) fn is_editing() -> bool {
    EDITING.load(Ordering::Acquire)
}
pub(super) fn accepts_notification(action: usize, generation: usize) -> bool {
    generation == GENERATION.load(Ordering::Acquire)
        && super::commands::Command::from_id(action).is_some_and(|c| c.is_shortcut())
}
#[cfg(test)]
pub(super) fn generation() -> usize {
    GENERATION.load(Ordering::Acquire)
}
const WAKE: u32 = WM_APP + 176;
const MODIFIER_KEYS: [usize; 8] = [0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0x5b, 0x5c];
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    action: usize,
    modifiers: u32,
    vk: usize,
    mode: KeyMode,
}
fn bindings(values: &[(usize, Hotkey)]) -> Result<Vec<Binding>, String> {
    let mut result: Vec<Binding> = Vec::new();
    for (action, key) in values.iter().filter(|(_, k)| k.is_assigned()) {
        let vk = key.virtual_key()? as usize;
        let modifiers = key.modifiers();
        if result
            .iter()
            .any(|b| b.vk == vk && b.modifiers == modifiers)
        {
            return Err("同じキーを複数の操作に割り当てられません".into());
        }
        result.push(Binding {
            action: *action,
            modifiers,
            vk,
            mode: key.mode,
        });
    }
    Ok(result)
}
struct KeyState {
    bindings: Vec<Binding>,
    down: [bool; 256],
    suppressed: [bool; 256],
    suppress_current: bool,
}
impl KeyState {
    fn new() -> Self {
        Self {
            bindings: vec![],
            down: [false; 256],
            suppressed: [false; 256],
            suppress_current: false,
        }
    }
    fn modifiers(&self) -> u32 {
        (self.down[0xa4] || self.down[0xa5]) as u32
            | (((self.down[0xa2] || self.down[0xa3]) as u32) << 1)
            | (((self.down[0xa0] || self.down[0xa1]) as u32) << 2)
            | (((self.down[0x5b] || self.down[0x5c]) as u32) << 3)
    }
    fn event(&mut self, vk: usize, pressed: bool) -> Vec<usize> {
        self.suppress_current = false;
        if vk >= 256 {
            return vec![];
        }
        // 割当変更や修飾キーの先行解放をまたいでも、抑止した押下と解放を対にする。
        if self.suppressed[vk] {
            self.suppress_current = true;
            self.down[vk] = pressed;
            self.suppressed[vk] = pressed;
            return vec![];
        }
        let relevant = self.down[vk]
            || MODIFIER_KEYS.contains(&vk)
            || self.bindings.iter().any(|b| b.vk == vk);
        if !relevant {
            return vec![];
        }
        let repeated = self.down[vk];
        self.down[vk] = pressed;
        let modifiers = self.modifiers();
        let mut events = vec![];
        if pressed && !repeated {
            if let Some(b) = self
                .bindings
                .iter()
                .find(|b| b.vk == vk && b.modifiers == modifiers)
                .cloned()
            {
                events.push(b.action);
                if b.mode == KeyMode::Override {
                    self.suppressed[vk] = true;
                    self.suppress_current = true;
                }
            }
        }
        events
    }
    fn replace_bindings(
        &mut self,
        values: Vec<Binding>,
        mut is_down: impl FnMut(usize) -> bool,
    ) -> Vec<usize> {
        for b in &values {
            if !self.bindings.iter().any(|old| old.vk == b.vk) {
                self.down[b.vk] = is_down(b.vk)
            }
        }
        self.bindings = values;
        vec![]
    }
    fn reconcile(&mut self, mut is_down: impl FnMut(usize) -> bool) -> Vec<usize> {
        let mut events = vec![];
        let keys: Vec<_> = self
            .bindings
            .iter()
            .map(|b| b.vk)
            .chain(MODIFIER_KEYS)
            .collect();
        for vk in keys {
            // 抑止した主キーはOS側の非同期状態へ反映されないため、実解放だけで解除する。
            if self.down[vk] && !self.suppressed[vk] && !is_down(vk) {
                events.extend(self.event(vk, false))
            }
        }
        events
    }
}
struct HookState {
    target: HWND,
    message: u32,
    keys: KeyState,
}
thread_local! {static STATE:RefCell<Option<HookState>>=const{RefCell::new(None)};}
fn post(state: &HookState, events: Vec<usize>, generation: usize) -> bool {
    let mut delivered = true;
    for action in events {
        if !accepts_notification(action, generation) {
            delivered = false;
            continue;
        }
        unsafe {
            delivered &= PostMessageW(
                Some(state.target),
                state.message,
                WPARAM(action),
                LPARAM(generation as isize),
            )
            .is_ok();
        }
    }
    delivered
}

fn mask_modifier_menu() {
    // 主キーを抑止するとAlt/Windows単独押下になるため、未割当VKでメニュー起動を防ぐ。
    let inputs = [KEYBD_EVENT_FLAGS(0), KEYEVENTF_KEYUP].map(|flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0xe8),
                dwFlags: flags,
                ..Default::default()
            },
        },
    });
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
static ALLOW_TEST_INPUT: AtomicBool = AtomicBool::new(false);
fn accepts_input(injected: bool) -> bool {
    #[cfg(test)]
    if ALLOW_TEST_INPUT.load(Ordering::Relaxed) {
        return true;
    }
    !injected
}
unsafe extern "system" fn keyboard_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    let mut suppress = false;
    let mut mask_menu = false;
    if code == HC_ACTION as i32 && l.0 != 0 {
        let pressed = match w.0 as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
            WM_KEYUP | WM_SYSKEYUP => Some(false),
            _ => None,
        };
        let key = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        if let Some(pressed) = pressed
            .filter(|_| accepts_input(key.flags.contains(LLKHF_INJECTED)) && key.vkCode != 0xe8)
        {
            let generation = GENERATION.load(Ordering::Acquire);
            STATE.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    if let Some(state) = slot.as_mut() {
                        let events = state.keys.event(key.vkCode as usize, pressed);
                        let starts_override = !events.is_empty() && state.keys.suppress_current;
                        if !post(state, events, generation) && starts_override {
                            // 配送できなかった操作では元のキーを奪わない。
                            state.keys.suppressed[key.vkCode as usize] = false;
                            state.keys.suppress_current = false;
                        }
                        suppress = state.keys.suppress_current;
                        mask_menu = starts_override && suppress && state.keys.modifiers() & 9 != 0;
                    }
                }
            });
        }
    }
    if mask_menu {
        mask_modifier_menu();
    }
    if suppress {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, w, l) }
}

fn needs_refresh(remote: bool, foreground_changed: bool, elapsed: Duration) -> bool {
    remote && (foreground_changed || elapsed >= Duration::from_millis(500))
}
fn is_mstsc(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.is_ok()
            && String::from_utf16_lossy(&path[..len as usize])
                .rsplit(['\\', '/'])
                .next()
                .is_some_and(|n| n.eq_ignore_ascii_case("mstsc.exe"))
    }
}
fn install_hook() -> Result<HHOOK, String> {
    unsafe {
        let module = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(module.into()), 0)
            .map_err(|e| format!("キー検出を開始できません: {e}"))
    }
}
enum Command {
    Bindings(Vec<Binding>, mpsc::SyncSender<()>),
    Stop,
}
/// 名前は従来の内部APIを保つが、通常アプリとMSTSCの両方を観測する。
pub struct ShortcutObserver {
    commands: Sender<Command>,
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}
impl ShortcutObserver {
    pub fn new(hwnd: HWND, message: u32) -> Result<Self, String> {
        let (commands, receive) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let raw = hwnd.0 as usize;
        let worker = thread::Builder::new()
            .name("spk-shortcuts".into())
            .spawn(move || run(raw, message, receive, ready))
            .map_err(|e| e.to_string())?;
        match started.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                commands,
                thread_id,
                worker: Some(worker),
            }),
            status => {
                let _ = worker.join();
                Err(status
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| "キー検出が終了しました".into()))
            }
        }
    }
    pub fn set_bindings(&self, values: &[(usize, Hotkey)]) -> Result<(), String> {
        let values = bindings(values)?;
        let (applied, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Bindings(values, applied))
            .map_err(|_| "キー検出が終了しています".to_owned())?;
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WAKE, WPARAM(0), LPARAM(0));
        }
        receive
            .recv()
            .map_err(|_| "キー設定を反映できません".into())
    }
}
impl Drop for ShortcutObserver {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WAKE, WPARAM(0), LPARAM(0));
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn run(
    raw: usize,
    message: u32,
    commands: Receiver<Command>,
    ready: mpsc::SyncSender<Result<u32, String>>,
) {
    let mut keys = KeyState::new();
    for vk in MODIFIER_KEYS {
        keys.down[vk] = unsafe { GetAsyncKeyState(vk as i32) } < 0;
    }
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(HookState {
            target: HWND(raw as *mut _),
            message,
            keys,
        })
    });
    let Ok(mut hook) = install_hook().inspect_err(|e| {
        let _ = ready.send(Err(e.clone()));
    }) else {
        return;
    };
    let timer = unsafe { SetTimer(None, 0, 50, None) };
    if timer == 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(hook);
        }
        let _ = ready.send(Err("キー監視タイマーを開始できません".into()));
        return;
    }
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_err() {
        unsafe {
            let _ = KillTimer(None, timer);
            let _ = UnhookWindowsHookEx(hook);
        }
        return;
    }
    let mut last_refresh = Instant::now();
    let mut previous_foreground = HWND::default();
    'messages: loop {
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Bindings(values, applied) => STATE.with(|slot| {
                    GENERATION.fetch_add(1, Ordering::AcqRel);
                    if let Some(s) = slot.borrow_mut().as_mut() {
                        let events = s.keys.replace_bindings(
                            values,
                            |vk| unsafe { GetAsyncKeyState(vk as i32) } < 0,
                        );
                        post(s, events, GENERATION.load(Ordering::Acquire));
                    }
                    let _ = applied.send(());
                }),
                Command::Stop => break 'messages,
            }
        }
        let foreground = unsafe { GetForegroundWindow() };
        let remote = is_mstsc(foreground);
        if needs_refresh(
            remote,
            foreground != previous_foreground,
            last_refresh.elapsed(),
        ) {
            if let Ok(next) = install_hook() {
                unsafe {
                    let _ = UnhookWindowsHookEx(hook);
                }
                hook = next;
            }
            last_refresh = Instant::now();
        }
        previous_foreground = foreground;
        // 通常前景では取り逃した解放も回収。MSTSC中の非同期キー状態は使わない。
        if !remote {
            STATE.with(|slot| {
                if let Some(s) = slot.borrow_mut().as_mut() {
                    let events = s
                        .keys
                        .reconcile(|vk| unsafe { GetAsyncKeyState(vk as i32) } < 0);
                    post(s, events, GENERATION.load(Ordering::Acquire));
                }
            });
        }
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
    }
    STATE.with(|slot| {
        if let Some(s) = slot.borrow_mut().as_mut() {
            let events = s.keys.replace_bindings(vec![], |_| false);
            post(s, events, GENERATION.load(Ordering::Acquire));
        }
    });
    unsafe {
        let _ = KillTimer(None, timer);
        let _ = UnhookWindowsHookEx(hook);
    }
    STATE.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_modifiers_repeat_rebinding_and_missing_release_are_safe() {
        let key = crate::key_binding::default_hotkeys()[0].clone();
        let mut state = KeyState::new();
        state.replace_bindings(bindings(&[(1, key.clone())]).unwrap(), |_| false);
        assert!(state.event(0x70, true).is_empty());
        state.event(0x70, false);
        state.event(0xa0, true);
        assert_eq!(state.event(0x70, true), [1]);
        assert!(state.event(0x70, true).is_empty());
        state.event(0x70, false);
        state.event(0xa4, true);
        assert!(state.event(0x70, true).is_empty());
        state.reconcile(|_| false);
        state.event(0xa1, true);
        assert_eq!(state.event(0x70, true), [1]);
        state.replace_bindings(bindings(&[(2, key)]).unwrap(), |_| true);
        assert!(state.event(0x70, true).is_empty());
        state.event(0x70, false);
        assert_eq!(state.event(0x70, true), [2]);
    }
    #[test]
    fn held_new_assignment_requires_release_and_unassigned_keys_are_not_bound() {
        let mut state = KeyState::new();
        let keys = crate::key_binding::default_hotkeys();
        assert!(bindings(&[(1, keys[4].clone())]).unwrap().is_empty());
        state.replace_bindings(bindings(&[(1, keys[0].clone())]).unwrap(), |_| true);
        assert!(state.event(0x70, true).is_empty());
        state.event(0x70, false);
        state.event(0xa0, true);
        assert_eq!(state.event(0x70, true), [1]);
    }

    #[test]
    fn override_balances_repeats_release_and_rebinding_without_suppressing_other_keys() {
        let mut key = crate::key_binding::default_hotkeys()[0].clone();
        key.mode = KeyMode::Override;
        let mut state = KeyState::new();
        state.replace_bindings(bindings(&[(4, key)]).unwrap(), |_| false);
        state.event(0x70, true); // 修飾不一致は通過。
        assert!(!state.suppress_current);
        state.event(0x70, false);
        state.event(0xa0, true);
        assert_eq!(state.event(0x70, true), [4]);
        assert!(state.suppress_current);
        state.reconcile(|_| false);
        assert!(state.suppressed[0x70]);
        state.event(0xa0, false);
        assert!(!state.suppress_current);
        state.replace_bindings(vec![], |_| false);
        assert!(state.event(0x70, true).is_empty());
        assert!(state.suppress_current);
        state.event(0x70, false);
        assert!(state.suppress_current);
        state.event(0x70, true);
        assert!(!state.suppress_current);
    }

    #[test]
    fn coexist_is_transparent_and_remote_refresh_is_immediate_then_periodic() {
        let key = crate::key_binding::default_hotkeys()[0].clone();
        let mut state = KeyState::new();
        state.replace_bindings(bindings(&[(1, key)]).unwrap(), |_| false);
        state.event(0xa1, true);
        assert_eq!(state.event(0x70, true), [1]);
        assert!(!state.suppress_current);
        state.event(0x70, false);
        assert!(!state.suppress_current);
        assert!(needs_refresh(true, true, Duration::ZERO));
        assert!(!needs_refresh(true, false, Duration::from_millis(499)));
        assert!(needs_refresh(true, false, Duration::from_millis(500)));
        assert!(!needs_refresh(false, true, Duration::from_secs(1)));
        assert!(!accepts_input(true));
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    #[test]
    fn editing_keeps_shortcuts_available_but_rebinding_rejects_old_notifications() {
        set_editing(false);
        let before = GENERATION.load(Ordering::Acquire);
        assert!(accepts_notification(4, before));
        set_editing(true);
        let editing = GENERATION.load(Ordering::Acquire);
        assert!(accepts_notification(4, before));
        assert!(accepts_notification(4, editing));
        assert!(accepts_notification(100, editing));
        assert!(accepts_notification(104, editing));
        assert!(!accepts_notification(5, editing));
        assert!(!accepts_notification(101, editing));
        set_editing(false);
        assert!(accepts_notification(4, editing));
        assert!(accepts_notification(100, editing));
        let current = GENERATION.load(Ordering::Acquire);
        assert!(accepts_notification(4, current));
        GENERATION.fetch_add(1, Ordering::AcqRel);
        assert!(!accepts_notification(4, current));
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use windows::core::PCWSTR;
    static DOWN: AtomicUsize = AtomicUsize::new(0);
    static UP: AtomicUsize = AtomicUsize::new(0);
    const MESSAGE: u32 = WM_APP + 198;
    unsafe extern "system" fn probe(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && l.0 != 0 {
            let key = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
            if key.vkCode == 0x87 {
                match w.0 as u32 {
                    WM_KEYDOWN | WM_SYSKEYDOWN => {
                        DOWN.fetch_add(1, Ordering::Relaxed);
                    }
                    WM_KEYUP | WM_SYSKEYUP => {
                        UP.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
        unsafe { CallNextHookEx(None, code, w, l) }
    }
    fn send(vk: u16, up: bool) {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        };
        assert_eq!(
            unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) },
            1
        );
    }
    fn pump(hwnd: HWND) -> usize {
        let until = Instant::now() + Duration::from_millis(150);
        let mut count = 0;
        while Instant::now() < until {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.hwnd == hwnd && msg.message == MESSAGE {
                        assert_eq!(msg.wParam.0, 4);
                        count += 1;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        count
    }
    struct Cleanup {
        hook: HHOOK,
        hwnd: HWND,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            // パニック時にも試験キーを解放し、注入許可とフックを残さない。
            for vk in [0x87, 0xa0, 0xa2] {
                send(vk, true);
            }
            pump(self.hwnd);
            ALLOW_TEST_INPUT.store(false, Ordering::Release);
            unsafe {
                let _ = UnhookWindowsHookEx(self.hook);
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
    #[test]
    #[ignore = "実Win32フックチェーンへ専用Ctrl+Shift+F24を合成入力する。実接続・物理キー試験とは区別する"]
    fn coexist_and_override_deliver_once_and_balance_the_real_hook_chain() {
        unsafe {
            let module = GetModuleHandleW(None).unwrap();
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("STATIC"),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(module.into()),
                None,
            )
            .unwrap();
            let hook =
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(probe), Some(module.into()), 0).unwrap();
            let _cleanup = Cleanup { hook, hwnd };
            let observer = ShortcutObserver::new(hwnd, MESSAGE).unwrap();
            let mut key = crate::key_binding::default_hotkeys()[0].clone();
            key.ctrl = true;
            key.key = "F24".into();
            ALLOW_TEST_INPUT.store(true, Ordering::Release);
            for (mode, expected_down, expected_up) in
                [(KeyMode::Coexist, 2, 1), (KeyMode::Override, 0, 0)]
            {
                key.mode = mode;
                observer.set_bindings(&[(4, key.clone())]).unwrap();
                DOWN.store(0, Ordering::Relaxed);
                UP.store(0, Ordering::Relaxed);
                send(0xa2, false);
                send(0xa0, false);
                send(0x87, false);
                send(0x87, false);
                send(0xa0, true);
                send(0xa2, true);
                send(0x87, true);
                assert_eq!(pump(hwnd), 1);
                assert_eq!(
                    (DOWN.load(Ordering::Relaxed), UP.load(Ordering::Relaxed)),
                    (expected_down, expected_up)
                );
            }
            // 上書きした主キーを保持したまま割当を削除しても反復・解放を漏らさない。
            send(0xa2, false);
            send(0xa0, false);
            send(0x87, false);
            assert_eq!(pump(hwnd), 1);
            observer.set_bindings(&[]).unwrap();
            send(0xa0, true);
            send(0xa2, true);
            send(0x87, false);
            send(0x87, true);
            assert_eq!(pump(hwnd), 0);
            assert_eq!(
                (DOWN.load(Ordering::Relaxed), UP.load(Ordering::Relaxed)),
                (0, 0)
            );
            send(0x87, false);
            send(0x87, true);
            assert_eq!(pump(hwnd), 0);
            assert_eq!(
                (DOWN.load(Ordering::Relaxed), UP.load(Ordering::Relaxed)),
                (1, 1)
            );
            drop(observer);
        }
    }
}
