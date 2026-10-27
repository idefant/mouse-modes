//! Windows implementation: LL keyboard suppression + unaccelerated raw mouse input.
//! The worker owns every native resource, including restoration of temporary SPI speed.
use crate::{EngineEvent, EngineStatus, Hotkey, Settings};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    mem::{size_of, size_of_val, zeroed},
    ptr::{null, null_mut},
    sync::{mpsc, Arc, Mutex},
    thread,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{
        LibraryLoader::GetModuleHandleW, StationsAndDesktops::*, Threading::GetCurrentThreadId,
    },
    UI::{
        Input::{KeyboardAndMouse::*, *},
        WindowsAndMessaging::*,
    },
};

const WAKE: u32 = WM_APP + 71;
const APPLY_MODES: u32 = WM_APP + 72;
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
    events: mpsc::Sender<EngineEvent>,
    status: Arc<Mutex<EngineStatus>>,
}
#[derive(Clone, Copy, Default)]
struct DesiredModes {
    speed: i32,
    horizontal: bool,
    vertical: bool,
}
struct Output {
    speed: i32,
    base_speed: Option<i32>,
    horizontal: bool,
    vertical: bool,
    accum_x: i32,
    accum_y: i32,
    clipped: bool,
    previous_clip: RECT,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }
thread_local! { static OUTPUT: RefCell<Option<Output>> = const { RefCell::new(None) }; }
thread_local! { static DESIRED: Cell<DesiredModes> = const { Cell::new(DesiredModes { speed: 0, horizontal: false, vertical: false }) }; }
fn with_output(action: impl FnOnce(&mut Output)) -> bool {
    let output = OUTPUT.with(|cell| cell.borrow_mut().take());
    let Some(mut output) = output else {
        return false;
    };
    // Native calls may run another window procedure or keyboard hook. Neither
    // TLS RefCell may stay borrowed across those calls.
    action(&mut output);
    OUTPUT.with(|cell| *cell.borrow_mut() = Some(output));
    true
}
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
        DESIRED.with(|cell| {
            cell.set(DesiredModes {
                speed: desired,
                horizontal,
                vertical,
            });
        });
        // SPI, ClipCursor and SendInput can synchronously reenter the keyboard
        // hook. Apply output after returning from the hook and releasing STATE.
        unsafe {
            PostThreadMessageW(GetCurrentThreadId(), APPLY_MODES, 0, 0);
        }
    }
}
impl Output {
    fn apply(&mut self, desired_modes: DesiredModes) {
        let DesiredModes {
            speed: desired,
            horizontal,
            vertical,
        } = desired_modes;
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
}
impl State {
    // A release can be lost when input moves to the secure desktop or lock screen.
    fn reconcile_key_state(&mut self, desktop_active: bool, is_down: impl Fn(i32) -> bool) {
        if !desktop_active {
            self.virtual_keys.clear();
            self.held.clear();
            self.suppressed.clear();
            if self.capture.take().is_some() {
                self.emit_message(false, "Capture interrupted. Record the combination again.");
            }
            self.modes();
            return;
        }
        let released: Vec<String> = self
            .virtual_keys
            .iter()
            // Suppressed keydowns do not update Windows' async key state.
            // Only a hook keyup (or desktop reset above) can release them.
            .filter(|(key, vk)| !self.suppressed.contains(*key) && !is_down(**vk))
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
}
impl Output {
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
unsafe fn input_desktop_active() -> bool {
    let input = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
    if input.is_null() {
        return false;
    }
    let desktop_name = |desktop| {
        let mut name = [0u16; 256];
        let mut needed = 0;
        if GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr() as _,
            size_of_val(&name) as u32,
            &mut needed,
        ) == 0
        {
            None
        } else {
            Some(name)
        }
    };
    let input_name = desktop_name(input);
    let thread_name = desktop_name(GetThreadDesktop(GetCurrentThreadId()));
    CloseDesktop(input);
    input_name.is_some() && input_name == thread_name
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
    // Keyboard software, remappers and remote sessions can send injected keys
    // (including F13-F24). We only inject mouse input ourselves, so accepting
    // these keyboard events cannot feed our own output back into this hook.
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
    if (message == WM_INPUT_DEVICE_CHANGE && wparam == GIDC_REMOVAL as WPARAM)
        || (message == WM_DEVICECHANGE && wparam == DBT_DEVICEREMOVECOMPLETE as WPARAM)
    {
        STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                state.reconcile_key_state(false, |_| false);
            }
        });
        return 0;
    }
    if message == WM_TIMER && wparam == 1 {
        let keys = STATE.with(|cell| {
            cell.borrow().as_ref().and_then(|state| {
                if state.held.is_empty() && state.capture.is_none() {
                    None
                } else {
                    Some(state.virtual_keys.values().copied().collect::<Vec<_>>())
                }
            })
        });
        let Some(keys) = keys else {
            return 0;
        };
        // Sample Win32 outside STATE: these calls may deliver another hook event.
        let desktop_active = input_desktop_active();
        let down: HashMap<_, _> = keys
            .into_iter()
            .map(|vk| (vk, GetAsyncKeyState(vk) < 0))
            .collect();
        STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                state.reconcile_key_state(desktop_active, |vk| *down.get(&vk).unwrap_or(&true));
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
                with_output(|state| {
                    if state.horizontal {
                        state.accum_x = state.accum_x.saturating_add(mouse.lLastX);
                        wheel(&mut state.accum_x, true);
                    }
                    if state.vertical {
                        state.accum_y = state.accum_y.saturating_add(mouse.lLastY);
                        wheel(&mut state.accum_y, false);
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
            events,
            status,
        })
    });
    OUTPUT.with(|s| {
        *s.borrow_mut() = Some(Output {
            speed: 0,
            base_speed: None,
            horizontal: false,
            vertical: false,
            accum_x: 0,
            accum_y: 0,
            clipped: false,
            previous_clip: zeroed(),
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
    // Do not register raw keyboard input: it disables the LL keyboard hook
    // while this process has focus (including the settings WebView).
    let devices = [2].map(|usage| RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: usage,
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    });
    let raw_input_ready = !hwnd.is_null()
        && RegisterRawInputDevices(
            devices.as_ptr(),
            devices.len() as u32,
            size_of::<RAWINPUTDEVICE>() as u32,
        ) != 0;
    // Receive unplug notifications without registering raw keyboard input.
    let mut filter: DEV_BROADCAST_DEVICEINTERFACE_W = zeroed();
    filter.dbcc_size = size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32;
    filter.dbcc_devicetype = DBT_DEVTYP_DEVICEINTERFACE;
    let notifications = if hwnd.is_null() {
        null_mut()
    } else {
        RegisterDeviceNotificationW(
            hwnd,
            &filter as *const _ as _,
            DEVICE_NOTIFY_WINDOW_HANDLE | DEVICE_NOTIFY_ALL_INTERFACE_CLASSES,
        )
    };
    // Install last so device initialization cannot delay the hook's message pump.
    let mut hook = if raw_input_ready {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), instance, 0)
    } else {
        null_mut()
    };
    let success = raw_input_ready && !hook.is_null() && !notifications.is_null();
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
            if message.message == APPLY_MODES {
                let desired = DESIRED.with(Cell::get);
                if !with_output(|output| output.apply(desired)) {
                    PostThreadMessageW(GetCurrentThreadId(), APPLY_MODES, 0, 0);
                }
            } else if message.message == WAKE {
                while let Ok(request) = rx.try_recv() {
                    let stop = matches!(request.command, Command::Stop);
                    if matches!(request.command, Command::Capture) {
                        // Windows silently removes LL hooks after a timeout. An old
                        // non-null handle does not prove that input is still arriving.
                        let replacement =
                            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), instance, 0);
                        if replacement.is_null() {
                            let _ = request.done.send(Err(format!(
                                "Не удалось включить перехват клавиатуры: {}",
                                std::io::Error::last_os_error()
                            )));
                            continue;
                        }
                        UnhookWindowsHookEx(hook);
                        hook = replacement;
                    }
                    STATE.with(|cell| {
                        let mut s = cell.borrow_mut();
                        let s = s.as_mut().unwrap();
                        match request.command {
                            Command::Settings(settings) => {
                                s.settings = settings;
                                s.modes();
                            }
                            Command::Capture => {
                                // A fresh hook must not inherit keys whose release
                                // was lost while the previous hook was inactive.
                                s.held.clear();
                                s.virtual_keys.clear();
                                s.suppressed.clear();
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
                            Command::Stop => {}
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
        cell.borrow_mut().take();
    });
    let output = OUTPUT.with(|cell| cell.borrow_mut().take());
    if let Some(mut output) = output {
        output.cleanup();
    }
    if !hook.is_null() {
        UnhookWindowsHookEx(hook);
    }
    if !notifications.is_null() {
        UnregisterDeviceNotification(notifications);
    }
    if !hwnd.is_null() {
        let remove = [2].map(|usage| RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: usage,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: null_mut(),
        });
        RegisterRawInputDevices(
            remove.as_ptr(),
            remove.len() as u32,
            size_of::<RAWINPUTDEVICE>() as u32,
        );
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
    // Programmable buttons may supply no scan code or retain the original key's
    // scan code. The virtual key is authoritative for function keys.
    if (VK_F1 as u32..=VK_F24 as u32).contains(&event.vkCode) {
        return format!("F{}", event.vkCode - VK_F1 as u32 + 1);
    }
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
            return format!(
                "WindowsScan{}{}",
                event.scanCode,
                if extended { "Extended" } else { "" }
            )
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

    #[test]
    fn programmable_f17_does_not_depend_on_the_scan_code() {
        for scan_code in [0, 0x1e, 0x68] {
            let event = KBDLLHOOKSTRUCT {
                vkCode: VK_F17 as u32,
                scanCode: scan_code,
                flags: LLKHF_INJECTED,
                time: 0,
                dwExtraInfo: 0,
            };
            assert_eq!(key_code(&event), "F17");
        }
    }

    #[test]
    fn injected_f17_is_suppressed_and_recorded_on_release() {
        let (events, received) = mpsc::channel();
        STATE.with(|cell| {
            *cell.borrow_mut() = Some(State {
                settings: Settings::default(),
                held: HashSet::new(),
                virtual_keys: HashMap::new(),
                suppressed: HashSet::new(),
                capture: Some(HashSet::new()),
                capture_invalid: false,
                events,
                status: Arc::new(Mutex::new(EngineStatus {
                    available: true,
                    capturing: true,
                    message: String::new(),
                })),
            });
        });
        let event = KBDLLHOOKSTRUCT {
            vkCode: VK_F17 as u32,
            scanCode: 0,
            flags: LLKHF_INJECTED,
            time: 0,
            dwExtraInfo: 0,
        };
        unsafe {
            let data = &event as *const _ as LPARAM;
            assert_eq!(keyboard(HC_ACTION as i32, WM_KEYDOWN as WPARAM, data), 1);
            assert!(received.try_recv().is_err());
            STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                let state = state.as_mut().unwrap();
                // Simulate the timer while Windows still reports a suppressed
                // injected F17 as up. Recording must wait for the real keyup.
                state.reconcile_key_state(true, |_| false);
                assert!(state.capture.is_some());
                assert!(state.held.contains("F17"));
            });
            assert!(received.try_recv().is_err());
            assert_eq!(keyboard(HC_ACTION as i32, WM_KEYUP as WPARAM, data), 1);
        }
        let captured = received.try_recv().unwrap();
        assert!(matches!(captured, EngineEvent::Captured(h) if h == chord(&["F17"])));
        STATE.with(|cell| {
            let mut state = cell.borrow_mut().take().unwrap();
            assert!(state.held.is_empty());
            assert!(state.suppressed.is_empty());
            assert!(state.capture.is_none());

            // A desktop switch still releases suppressed keys and cancels input.
            state.capture = Some(held(&["F17"]));
            state.held.insert("F17".into());
            state.suppressed.insert("F17".into());
            state.virtual_keys.insert("F17".into(), VK_F17 as i32);
            state.reconcile_key_state(false, |_| true);
            assert!(state.held.is_empty());
            assert!(state.virtual_keys.is_empty());
            assert!(state.suppressed.is_empty());
            assert!(state.capture.is_none());
        });
    }

    #[test]
    #[ignore = "Sends F17 through the live Windows input system; run on an idle desktop"]
    fn native_injected_f17_capture_survives_timer() {
        let (send, received) = mpsc::channel();
        let engine = EngineHandle::start(
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
            Settings::default(),
        )
        .unwrap();
        engine.begin_capture().unwrap();
        unsafe {
            let mut input: INPUT = zeroed();
            input.r#type = INPUT_KEYBOARD;
            input.Anonymous.ki.wVk = VK_F17;
            let pressed = SendInput(1, &input, size_of::<INPUT>() as i32);
            thread::sleep(std::time::Duration::from_millis(350));
            let still_capturing = engine.status().capturing;
            input.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
            let released = SendInput(1, &input, size_of::<INPUT>() as i32);
            assert_eq!(pressed, 1, "F17 keydown injection failed");
            assert_eq!(released, 1, "F17 keyup injection failed");
            assert!(still_capturing, "timer interrupted the held F17 capture");
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let event = received
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("no capture result from the live Windows keyboard hook");
            if let EngineEvent::Captured(hotkey) = event {
                assert_eq!(hotkey, chord(&["F17"]));
                break;
            }
        }
        engine.shutdown();
    }

    #[test]
    #[ignore = "Temporarily clips the real cursor; run on an idle desktop"]
    fn native_scroll_release_restores_cursor() {
        unsafe fn clip() -> (i32, i32, i32, i32) {
            let mut rect: RECT = zeroed();
            assert_ne!(GetClipCursor(&mut rect), 0);
            (rect.left, rect.top, rect.right, rect.bottom)
        }
        unsafe fn key(vk: u16, up: bool) {
            let mut input: INPUT = zeroed();
            input.r#type = INPUT_KEYBOARD;
            input.Anonymous.ki.wVk = vk;
            input.Anonymous.ki.dwFlags = if up { KEYEVENTF_KEYUP } else { 0 };
            assert_eq!(SendInput(1, &input, size_of::<INPUT>() as i32), 1);
        }
        let mut settings = Settings::default();
        settings.hotkeys.scroll_horizontal = Some(chord(&["F15"]));
        settings.hotkeys.scroll_vertical = Some(chord(&["F16"]));
        settings.hotkeys.scroll_both = Some(chord(&["F15", "F16"]));
        let original = unsafe { clip() };
        let engine = EngineHandle::start(Arc::new(|_| {}), settings).unwrap();
        let mut results = Vec::new();
        for keys in [vec![VK_F15], vec![VK_F16], vec![VK_F15, VK_F16]] {
            unsafe {
                for &vk in &keys {
                    key(vk, false);
                }
                thread::sleep(std::time::Duration::from_millis(350));
                let held_clip = clip();
                let mut motion: INPUT = zeroed();
                motion.r#type = INPUT_MOUSE;
                motion.Anonymous.mi.dwFlags = MOUSEEVENTF_MOVE;
                motion.Anonymous.mi.dx = 50;
                motion.Anonymous.mi.dy = 30;
                SendInput(1, &motion, size_of::<INPUT>() as i32);
                for &vk in keys.iter().rev() {
                    key(vk, true);
                }
                thread::sleep(std::time::Duration::from_millis(150));
                results.push((keys, held_clip, clip()));
            }
        }
        // Queue down/up together: a native cursor call in the keydown callback
        // can synchronously deliver keyup and reenter the keyboard handler.
        let rapid = unsafe {
            let mut inputs: [INPUT; 2] = zeroed();
            for input in &mut inputs {
                input.r#type = INPUT_KEYBOARD;
                input.Anonymous.ki.wVk = VK_F15;
            }
            inputs[1].Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
            let mut sent = 0;
            for _ in 0..100 {
                sent += SendInput(2, inputs.as_ptr(), size_of::<INPUT>() as i32);
            }
            thread::sleep(std::time::Duration::from_millis(150));
            (sent, clip())
        };
        engine.shutdown();
        assert_eq!(rapid.0, 200, "rapid scroll key injection failed");
        assert_eq!(rapid.1, original, "rapid down/up left the cursor clipped");
        for (keys, held_clip, released_clip) in results {
            assert_eq!(
                held_clip.2 - held_clip.0,
                1,
                "mode did not activate: {keys:?}"
            );
            assert_eq!(
                held_clip.3 - held_clip.1,
                1,
                "mode did not activate: {keys:?}"
            );
            assert_eq!(
                released_clip, original,
                "cursor stayed clipped after release: {keys:?}"
            );
        }
    }
}
