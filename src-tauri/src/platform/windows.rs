//! Windows implementation: LL keyboard suppression + unaccelerated raw mouse input.
//! The worker owns every native resource, including restoration of temporary SPI speed.
use crate::{EngineEvent, EngineStatus, Hotkey, Settings};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{mpsc, Arc, Mutex},
    thread,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::{KeyboardAndMouse::*, *},
        WindowsAndMessaging::*,
    },
};

const WAKE: u32 = WM_APP + 71;
type Callback = Arc<dyn Fn(EngineEvent) + Send + Sync>;
enum Command {
    Settings(Settings),
    Capture,
    Cancel,
    Stop,
}
struct Request {
    command: Command,
    done: mpsc::SyncSender<Result<(), String>>,
}
pub struct EngineHandle {
    tx: mpsc::Sender<Request>,
    thread_id: u32,
    status: Arc<Mutex<EngineStatus>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl EngineHandle {
    pub fn start(callback: Callback, settings: Settings) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let status = Arc::new(Mutex::new(EngineStatus {
            available: false,
            message: "Starting Windows input engine".into(),
            capturing: false,
        }));
        let worker_status = status.clone();
        let worker =
            thread::spawn(move || unsafe { run(rx, settings, callback, worker_status, ready_tx) });
        match ready_rx.recv().map_err(|e| e.to_string())? {
            Ok(thread_id) => Ok(Self {
                tx,
                thread_id,
                status,
                worker: Mutex::new(Some(worker)),
            }),
            Err(error) => {
                let _ = worker.join();
                Err(error)
            }
        }
    }
    fn request(&self, command: Command) -> Result<(), String> {
        let (done, result) = mpsc::sync_channel(1);
        self.tx
            .send(Request { command, done })
            .map_err(|e| e.to_string())?;
        unsafe {
            if PostThreadMessageW(self.thread_id, WAKE, 0, 0) == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        result.recv().map_err(|e| e.to_string())?
    }
    pub fn update_settings(&self, settings: Settings) -> Result<(), String> {
        self.request(Command::Settings(settings))
    }
    pub fn begin_capture(&self) -> Result<(), String> {
        self.request(Command::Capture)
    }
    pub fn cancel_capture(&self) -> Result<(), String> {
        self.request(Command::Cancel)
    }
    pub fn status(&self) -> EngineStatus {
        self.status.lock().unwrap().clone()
    }
    pub fn shutdown(&self) {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = self.request(Command::Stop);
            let _ = worker.join();
        }
    }
}
impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
struct State {
    settings: Settings,
    held: HashSet<String>,
    virtual_keys: HashMap<String, i32>,
    suppressed: HashSet<String>,
    capture: Option<HashSet<String>>,
    capture_invalid: bool,
    speed: i32,
    base_speed: Option<i32>,
    horizontal: bool,
    vertical: bool,
    accum_x: i32,
    accum_y: i32,
    clipped: bool,
    previous_clip: RECT,
    events: mpsc::Sender<EngineEvent>,
    status: Arc<Mutex<EngineStatus>>,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }
impl State {
    fn emit_status(&self, capturing: bool) {
        self.emit_message(
            capturing,
            "Windows: keyboard hook and raw mouse input active",
        );
    }
    fn emit_message(&self, capturing: bool, message: &str) {
        let status = EngineStatus {
            available: true,
            message: message.into(),
            capturing,
        };
        *self.status.lock().unwrap() = status.clone();
        let _ = self.events.send(EngineEvent::Status(status));
    }
    fn modes(&mut self) {
        let hotkeys = &self.settings.hotkeys;
        let enabled = self.capture.is_none();
        let active = |h: &Option<Hotkey>| {
            enabled && h.as_ref().is_some_and(|h| matches_chord(h, &self.held))
        };
        let desired = if active(&hotkeys.fast) {
            1
        } else if active(&hotkeys.slow) {
            -1
        } else {
            0
        };
        let both = active(&hotkeys.scroll_both);
        let horizontal = both || active(&hotkeys.scroll_horizontal);
        let vertical = both || active(&hotkeys.scroll_vertical);
        unsafe {
            if desired != self.speed {
                if desired != 0 && self.base_speed.is_none() {
                    let mut base = 0i32;
                    if SystemParametersInfoW(SPI_GETMOUSESPEED, 0, &mut base as *mut _ as _, 0) != 0
                    {
                        self.base_speed = Some(base);
                    }
                }
                if let Some(base) = self.base_speed {
                    let target = (base + desired * 10).clamp(1, 20);
                    SystemParametersInfoW(SPI_SETMOUSESPEED, 0, target as usize as _, 0);
                    if desired == 0 {
                        self.base_speed = None;
                    }
                }
                self.speed = desired;
            }
            if (horizontal || vertical) && !self.clipped {
                let mut anchor = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut anchor) != 0 && GetClipCursor(&mut self.previous_clip) != 0 {
                    let rect = RECT {
                        left: anchor.x,
                        top: anchor.y,
                        right: anchor.x + 1,
                        bottom: anchor.y + 1,
                    };
                    self.clipped = ClipCursor(&rect) != 0;
                }
                self.accum_x = 0;
                self.accum_y = 0;
            } else if !horizontal && !vertical && self.clipped {
                ClipCursor(&self.previous_clip);
                self.clipped = false;
            }
        }
        if !horizontal {
            self.accum_x = 0;
        }
        if !vertical {
            self.accum_y = 0;
        }
        self.horizontal = horizontal;
        self.vertical = vertical;
    }
    // A release can be lost on the secure desktop, lock, or device removal.
    unsafe fn reconcile_releases(&mut self) {
        let released: Vec<String> = self
            .virtual_keys
            .iter()
            .filter(|(_, vk)| GetAsyncKeyState(**vk) >= 0)
            .map(|(key, _)| key.clone())
            .collect();
        if released.is_empty() {
            return;
        }
        for key in released {
            self.virtual_keys.remove(&key);
            self.held.remove(&key);
            self.suppressed.remove(&key);
        }
        if self.capture.is_some() && self.held.is_empty() {
            self.capture = None;
            self.emit_message(false, "Capture interrupted. Record the combination again.");
        }
        self.modes();
    }
    fn cleanup(&mut self) {
        unsafe {
            if self.clipped {
                ClipCursor(&self.previous_clip);
                self.clipped = false;
            }
            if let Some(base) = self.base_speed.take() {
                SystemParametersInfoW(SPI_SETMOUSESPEED, 0, base as usize as _, 0);
            }
        }
    }
}
fn matches_chord(chord: &Hotkey, held: &HashSet<String>) -> bool {
    !chord.keys.is_empty()
        && chord.keys.iter().all(|key| match key.as_str() {
            "Control" | "Shift" | "Alt" | "Meta" => {
                held.contains(&format!("{key}Left")) || held.contains(&format!("{key}Right"))
            }
            _ => held.contains(key),
        })
}
fn chord_contains(chord: &Hotkey, key: &str) -> bool {
    chord.keys.iter().any(|configured| {
        configured == key
            || ["Control", "Shift", "Alt", "Meta"].iter().any(|modifier| {
                configured == modifier
                    && (key == format!("{modifier}Left") || key == format!("{modifier}Right"))
            })
    })
}
fn normalize(mut keys: HashSet<String>) -> Hotkey {
    for modifier in ["Control", "Shift", "Alt", "Meta"] {
        let left = format!("{modifier}Left");
        let right = format!("{modifier}Right");
        if keys.contains(&left) && keys.contains(&right) {
            keys.remove(&left);
            keys.remove(&right);
            keys.insert(modifier.into());
        }
    }
    let mut keys: Vec<_> = keys.into_iter().collect();
    keys.sort();
    Hotkey { keys }
}
unsafe extern "system" fn keyboard(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(null_mut(), code, message, data);
    }
    let event = &*(data as *const KBDLLHOOKSTRUCT);
    if event.flags & LLKHF_INJECTED != 0 {
        return CallNextHookEx(null_mut(), code, message, data);
    }
    let down = message as u32 == WM_KEYDOWN || message as u32 == WM_SYSKEYDOWN;
    let key = key_code(event);
    let suppress = STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let Some(state) = state.as_mut() else {
            return false;
        };
        let capturing = state.capture.is_some();
        if down {
            state.held.insert(key.clone());
            state.virtual_keys.insert(key.clone(), event.vkCode as i32);
        } else {
            state.held.remove(&key);
            state.virtual_keys.remove(&key);
        }
        if let Some(capture) = state.capture.as_mut() {
            if down {
                capture.insert(key.clone());
                state.suppressed.insert(key.clone());
                if !crate::engine::supported_key(&key) || capture.len() > 16 {
                    state.capture_invalid = true;
                }
            }
            if state.held.is_empty() && !capture.is_empty() {
                let captured = normalize(state.capture.take().unwrap());
                if state.capture_invalid {
                    state.emit_message(
                        false,
                        "Unsupported key or too many keys. Record another combination.",
                    );
                } else {
                    let _ = state.events.send(EngineEvent::Captured(captured));
                    state.emit_status(false);
                }
            }
        }
        state.modes();
        let hotkeys = &state.settings.hotkeys;
        let active = [
            &hotkeys.slow,
            &hotkeys.fast,
            &hotkeys.scroll_horizontal,
            &hotkeys.scroll_vertical,
            &hotkeys.scroll_both,
        ]
        .into_iter()
        .flatten()
        .any(|h| matches_chord(h, &state.held) && chord_contains(h, &key));
        if down && (capturing || active) {
            state.suppressed.insert(key.clone());
            return true;
        }
        if !down {
            return state.suppressed.remove(&key) || capturing;
        }
        capturing
    });
    if suppress {
        1
    } else {
        CallNextHookEx(null_mut(), code, message, data)
    }
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_TIMER && wparam == 1 {
        STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                state.reconcile_releases();
            }
        });
        return 0;
    }
    if message == WM_INPUT {
        let mut raw: RAWINPUT = zeroed();
        let mut size = size_of::<RAWINPUT>() as u32;
        if GetRawInputData(
            lparam as _,
            RID_INPUT,
            &mut raw as *mut _ as _,
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        ) != u32::MAX
            && raw.header.dwType == RIM_TYPEMOUSE
        {
            let mouse = raw.data.mouse;
            // Absolute tablet coordinates are not relative mouse displacement.
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
                STATE.with(|cell| {
                    if let Some(state) = cell.borrow_mut().as_mut() {
                        if state.horizontal {
                            state.accum_x = state.accum_x.saturating_add(mouse.lLastX);
                            wheel(&mut state.accum_x, true);
                        }
                        if state.vertical {
                            state.accum_y = state.accum_y.saturating_add(mouse.lLastY);
                            wheel(&mut state.accum_y, false);
                        }
                    }
                });
            }
        }
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}
unsafe fn wheel(accum: &mut i32, horizontal: bool) {
    let notches = (*accum / 20).clamp(-20, 20);
    if notches == 0 {
        return;
    }
    let mut input: INPUT = zeroed();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dwFlags = if horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };
    input.Anonymous.mi.mouseData = (notches * 120 * if horizontal { 1 } else { -1 }) as u32;
    if SendInput(1, &input, size_of::<INPUT>() as i32) == 1 {
        *accum -= notches * 20;
    }
}
unsafe fn run(
    rx: mpsc::Receiver<Request>,
    settings: Settings,
    callback: Callback,
    status: Arc<Mutex<EngineStatus>>,
    ready: mpsc::SyncSender<Result<u32, String>>,
) {
    let (events, event_rx) = mpsc::channel();
    // Keep LL callbacks below Windows' hook timeout: Tauri emission runs separately.
    let emitter = thread::spawn(move || {
        for event in event_rx {
            callback(event);
        }
    });
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            settings,
            held: HashSet::new(),
            virtual_keys: HashMap::new(),
            suppressed: HashSet::new(),
            capture: None,
            capture_invalid: false,
            speed: 0,
            base_speed: None,
            horizontal: false,
            vertical: false,
            accum_x: 0,
            accum_y: 0,
            clipped: false,
            previous_clip: zeroed(),
            events,
            status,
        })
    });
    let instance = GetModuleHandleW(null());
    let class: Vec<u16> = "MouseModesRawInput\0".encode_utf16().collect();
    let mut wc: WNDCLASSW = zeroed();
    wc.lpfnWndProc = Some(window_proc);
    wc.hInstance = instance;
    wc.lpszClassName = class.as_ptr();
    let atom = RegisterClassW(&wc);
    let hwnd = if atom == 0 {
        null_mut()
    } else {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null(),
        )
    };
    let hook = if hwnd.is_null() {
        null_mut()
    } else {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), instance, 0)
    };
    let device = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    let success = !hwnd.is_null()
        && !hook.is_null()
        && RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0;
    if !success {
        let _ = ready.send(Err(format!(
            "Windows input initialization failed: {}",
            std::io::Error::last_os_error()
        )));
    } else {
        SetTimer(hwnd, 1, 100, None);
        STATE.with(|s| s.borrow().as_ref().unwrap().emit_status(false));
        let _ = ready.send(Ok(GetCurrentThreadId()));
        let mut message: MSG = zeroed();
        'pump: while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if message.message == WAKE {
                while let Ok(request) = rx.try_recv() {
                    let stop = matches!(request.command, Command::Stop);
                    STATE.with(|cell| {
                        let mut s = cell.borrow_mut();
                        let s = s.as_mut().unwrap();
                        match request.command {
                            Command::Settings(settings) => {
                                s.settings = settings;
                                s.modes();
                            }
                            Command::Capture => {
                                s.capture_invalid = false;
                                s.capture = Some(HashSet::new());
                                s.modes();
                                s.emit_status(true);
                            }
                            Command::Cancel => {
                                s.capture = None;
                                s.modes();
                                s.emit_status(false);
                            }
                            Command::Stop => s.cleanup(),
                        }
                    });
                    let _ = request.done.send(Ok(()));
                    if stop {
                        break 'pump;
                    }
                }
            } else {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    STATE.with(|cell| {
        if let Some(mut s) = cell.borrow_mut().take() {
            s.cleanup();
        }
    });
    if !hook.is_null() {
        UnhookWindowsHookEx(hook);
    }
    if !hwnd.is_null() {
        let remove = RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: null_mut(),
        };
        RegisterRawInputDevices(&remove, 1, size_of::<RAWINPUTDEVICE>() as u32);
        KillTimer(hwnd, 1);
        DestroyWindow(hwnd);
    }
    if atom != 0 {
        UnregisterClassW(class.as_ptr(), instance);
    }
    let _ = emitter.join();
}

fn key_code(event: &KBDLLHOOKSTRUCT) -> String {
    let extended = event.flags & LLKHF_EXTENDED != 0;
    if event.vkCode == VK_SNAPSHOT as u32 {
        return "PrintScreen".into();
    }
    if event.vkCode == VK_PAUSE as u32 {
        return "Pause".into();
    }
    let code = match (event.scanCode, extended) {
        (0x1d, false) => "ControlLeft",
        (0x1d, true) => "ControlRight",
        (0x2a, _) => "ShiftLeft",
        (0x36, _) => "ShiftRight",
        (0x38, false) => "AltLeft",
        (0x38, true) => "AltRight",
        (0x5b, true) => "MetaLeft",
        (0x5c, true) => "MetaRight",
        (0x1c, true) => "NumpadEnter",
        (0x35, true) => "NumpadDivide",
        (0x47, true) => "Home",
        (0x48, true) => "ArrowUp",
        (0x49, true) => "PageUp",
        (0x4b, true) => "ArrowLeft",
        (0x4d, true) => "ArrowRight",
        (0x4f, true) => "End",
        (0x50, true) => "ArrowDown",
        (0x51, true) => "PageDown",
        (0x52, true) => "Insert",
        (0x53, true) => "Delete",
        (0x01, _) => "Escape",
        (0x02, _) => "Digit1",
        (0x03, _) => "Digit2",
        (0x04, _) => "Digit3",
        (0x05, _) => "Digit4",
        (0x06, _) => "Digit5",
        (0x07, _) => "Digit6",
        (0x08, _) => "Digit7",
        (0x09, _) => "Digit8",
        (0x0a, _) => "Digit9",
        (0x0b, _) => "Digit0",
        (0x0c, _) => "Minus",
        (0x0d, _) => "Equal",
        (0x0e, _) => "Backspace",
        (0x0f, _) => "Tab",
        (0x10, _) => "KeyQ",
        (0x11, _) => "KeyW",
        (0x12, _) => "KeyE",
        (0x13, _) => "KeyR",
        (0x14, _) => "KeyT",
        (0x15, _) => "KeyY",
        (0x16, _) => "KeyU",
        (0x17, _) => "KeyI",
        (0x18, _) => "KeyO",
        (0x19, _) => "KeyP",
        (0x1a, _) => "BracketLeft",
        (0x1b, _) => "BracketRight",
        (0x1c, false) => "Enter",
        (0x1e, _) => "KeyA",
        (0x1f, _) => "KeyS",
        (0x20, _) => "KeyD",
        (0x21, _) => "KeyF",
        (0x22, _) => "KeyG",
        (0x23, _) => "KeyH",
        (0x24, _) => "KeyJ",
        (0x25, _) => "KeyK",
        (0x26, _) => "KeyL",
        (0x27, _) => "Semicolon",
        (0x28, _) => "Quote",
        (0x29, _) => "Backquote",
        (0x2b, _) => "Backslash",
        (0x2c, _) => "KeyZ",
        (0x2d, _) => "KeyX",
        (0x2e, _) => "KeyC",
        (0x2f, _) => "KeyV",
        (0x30, _) => "KeyB",
        (0x31, _) => "KeyN",
        (0x32, _) => "KeyM",
        (0x33, _) => "Comma",
        (0x34, _) => "Period",
        (0x35, false) => "Slash",
        (0x37, false) => "NumpadMultiply",
        (0x39, _) => "Space",
        (0x3a, _) => "CapsLock",
        (0x45, _) => "NumLock",
        (0x46, _) => "ScrollLock",
        (0x47, false) => "Numpad7",
        (0x48, false) => "Numpad8",
        (0x49, false) => "Numpad9",
        (0x4a, false) => "NumpadSubtract",
        (0x4b, false) => "Numpad4",
        (0x4c, false) => "Numpad5",
        (0x4d, false) => "Numpad6",
        (0x4e, false) => "NumpadAdd",
        (0x4f, false) => "Numpad1",
        (0x50, false) => "Numpad2",
        (0x51, false) => "Numpad3",
        (0x52, false) => "Numpad0",
        (0x53, false) => "NumpadDecimal",
        (0x56, _) => "IntlBackslash",
        (0x5d, true) => "ContextMenu",
        _ => {
            return if (0x70..=0x87).contains(&event.vkCode) {
                format!("F{}", event.vkCode - 0x6f)
            } else {
                format!(
                    "WindowsScan{}{}",
                    event.scanCode,
                    if extended { "Extended" } else { "" }
                )
            }
        }
    };
    code.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chord(keys: &[&str]) -> Hotkey {
        Hotkey {
            keys: keys.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn held(keys: &[&str]) -> HashSet<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn side_specific_modifier_and_any_side_are_distinct() {
        assert!(!matches_chord(
            &chord(&["ControlLeft", "KeyQ"]),
            &held(&["ControlRight", "KeyQ"])
        ));
        for side in ["ControlLeft", "ControlRight"] {
            assert!(matches_chord(
                &chord(&["Control", "KeyQ"]),
                &held(&[side, "KeyQ"])
            ));
        }
        assert!(!matches_chord(
            &chord(&["Control", "KeyQ"]),
            &held(&["KeyQ"])
        ));
        assert!(!matches_chord(&chord(&[]), &held(&[])));
    }
    #[test]
    fn recording_two_modifier_sides_becomes_any_side() {
        assert_eq!(
            normalize(held(&["AltLeft", "AltRight", "KeyQ"])).keys,
            vec!["Alt", "KeyQ"]
        );
        assert_eq!(
            normalize(held(&["AltRight", "KeyQ"])).keys,
            vec!["AltRight", "KeyQ"]
        );
    }
    #[test]
    fn unrelated_keys_are_not_eaten_while_a_mode_is_held() {
        let h = chord(&["Control", "F13"]);
        assert!(chord_contains(&h, "F13"));
        assert!(chord_contains(&h, "ControlRight"));
        assert!(!chord_contains(&h, "KeyA"));
    }
    #[test]
    fn physical_codes_distinguish_sides_and_numpad() {
        let mut event: KBDLLHOOKSTRUCT = unsafe { zeroed() };
        event.scanCode = 0x1d;
        assert_eq!(key_code(&event), "ControlLeft");
        event.flags = LLKHF_EXTENDED;
        assert_eq!(key_code(&event), "ControlRight");
        event.scanCode = 0x1c;
        assert_eq!(key_code(&event), "NumpadEnter");
        event.flags = 0;
        assert_eq!(key_code(&event), "Enter");
    }
}
