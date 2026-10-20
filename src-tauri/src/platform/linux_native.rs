//! Native Linux input relay. The privileged helper has no GUI initialization.
//! Device construction/destruction belongs to the discovery worker, never the
//! bounded input event loop. Only intentional capture results cross stdout.
use crate::{EngineStatus, Hotkey, Settings};
use evdev::{
    raw_stream::RawDevice, uinput::VirtualDevice, AttributeSet, EventType, InputEvent,
    RelativeAxisCode as Rel, UinputAbsSetup,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

const KEY_NAMES: &[(u16, &str)] = &[
    (1, "Escape"),
    (2, "Digit1"),
    (3, "Digit2"),
    (4, "Digit3"),
    (5, "Digit4"),
    (6, "Digit5"),
    (7, "Digit6"),
    (8, "Digit7"),
    (9, "Digit8"),
    (10, "Digit9"),
    (11, "Digit0"),
    (12, "Minus"),
    (13, "Equal"),
    (14, "Backspace"),
    (15, "Tab"),
    (16, "KeyQ"),
    (17, "KeyW"),
    (18, "KeyE"),
    (19, "KeyR"),
    (20, "KeyT"),
    (21, "KeyY"),
    (22, "KeyU"),
    (23, "KeyI"),
    (24, "KeyO"),
    (25, "KeyP"),
    (26, "BracketLeft"),
    (27, "BracketRight"),
    (28, "Enter"),
    (29, "ControlLeft"),
    (30, "KeyA"),
    (31, "KeyS"),
    (32, "KeyD"),
    (33, "KeyF"),
    (34, "KeyG"),
    (35, "KeyH"),
    (36, "KeyJ"),
    (37, "KeyK"),
    (38, "KeyL"),
    (39, "Semicolon"),
    (40, "Quote"),
    (41, "Backquote"),
    (42, "ShiftLeft"),
    (43, "Backslash"),
    (44, "KeyZ"),
    (45, "KeyX"),
    (46, "KeyC"),
    (47, "KeyV"),
    (48, "KeyB"),
    (49, "KeyN"),
    (50, "KeyM"),
    (51, "Comma"),
    (52, "Period"),
    (53, "Slash"),
    (54, "ShiftRight"),
    (55, "NumpadMultiply"),
    (56, "AltLeft"),
    (57, "Space"),
    (58, "CapsLock"),
    (59, "F1"),
    (60, "F2"),
    (61, "F3"),
    (62, "F4"),
    (63, "F5"),
    (64, "F6"),
    (65, "F7"),
    (66, "F8"),
    (67, "F9"),
    (68, "F10"),
    (69, "NumLock"),
    (70, "ScrollLock"),
    (71, "Numpad7"),
    (72, "Numpad8"),
    (73, "Numpad9"),
    (74, "NumpadSubtract"),
    (75, "Numpad4"),
    (76, "Numpad5"),
    (77, "Numpad6"),
    (78, "NumpadAdd"),
    (79, "Numpad1"),
    (80, "Numpad2"),
    (81, "Numpad3"),
    (82, "Numpad0"),
    (83, "NumpadDecimal"),
    (87, "F11"),
    (88, "F12"),
    (96, "NumpadEnter"),
    (97, "ControlRight"),
    (98, "NumpadDivide"),
    (99, "PrintScreen"),
    (100, "AltRight"),
    (102, "Home"),
    (103, "ArrowUp"),
    (104, "PageUp"),
    (105, "ArrowLeft"),
    (106, "ArrowRight"),
    (107, "End"),
    (108, "ArrowDown"),
    (109, "PageDown"),
    (110, "Insert"),
    (111, "Delete"),
    (119, "Pause"),
    (125, "MetaLeft"),
    (126, "MetaRight"),
    (127, "ContextMenu"),
    (183, "F13"),
    (184, "F14"),
    (185, "F15"),
    (186, "F16"),
    (187, "F17"),
    (188, "F18"),
    (189, "F19"),
    (190, "F20"),
    (191, "F21"),
    (192, "F22"),
    (193, "F23"),
    (194, "F24"),
];
fn keyboard_key(code: u16) -> bool {
    code < 0x100 || code >= 0x160
}
fn key_name(code: u16) -> Option<&'static str> {
    KEY_NAMES
        .iter()
        .find_map(|(key, name)| (*key == code).then_some(*name))
}
fn normalize(mut keys: BTreeSet<String>) -> Hotkey {
    for modifier in ["Control", "Shift", "Alt", "Meta"] {
        let left = format!("{modifier}Left");
        let right = format!("{modifier}Right");
        if keys.contains(&left) && keys.contains(&right) {
            keys.remove(&left);
            keys.remove(&right);
            keys.insert(modifier.into());
        }
    }
    Hotkey {
        keys: keys.into_iter().collect(),
    }
}
fn matches(binding: &Option<Hotkey>, held: &BTreeSet<String>) -> bool {
    binding.as_ref().is_some_and(|binding| {
        !binding.keys.is_empty()
            && binding.keys.iter().all(|key| {
                if ["Control", "Shift", "Alt", "Meta"].contains(&key.as_str()) {
                    held.contains(&format!("{key}Left")) || held.contains(&format!("{key}Right"))
                } else {
                    held.contains(key)
                }
            })
    })
}
fn bindings(settings: &Settings) -> [&Option<Hotkey>; 5] {
    let h = &settings.hotkeys;
    [
        &h.slow,
        &h.fast,
        &h.scroll_horizontal,
        &h.scroll_vertical,
        &h.scroll_both,
    ]
}
#[derive(Default)]
struct Modes {
    state: [bool; 5],
    fraction: [f64; 2],
    scroll: [i64; 2],
}
impl Modes {
    fn update(&mut self, settings: &Settings, held: &BTreeSet<String>, disabled: bool) {
        let next = bindings(settings).map(|binding| !disabled && matches(binding, held));
        if next != self.state {
            self.fraction = [0.; 2];
            self.scroll = [0; 2];
        }
        self.state = next;
    }
    fn motion(&mut self, delta: [i64; 2]) -> [i32; 4] {
        let [slow, fast, horizontal, vertical, both] = self.state;
        let mut result = [0; 4];
        if horizontal || vertical || both {
            for (axis, active) in [horizontal || both, vertical || both]
                .into_iter()
                .enumerate()
            {
                if active {
                    self.scroll[axis] = self.scroll[axis].saturating_add(delta[axis]);
                    let steps = (self.scroll[axis] / 20).clamp(-20, 20);
                    self.scroll[axis] -= steps * 20;
                    result[axis + 2] = (if axis == 1 { -steps } else { steps }) as i32;
                } else {
                    self.scroll[axis] = 0;
                }
            }
        } else {
            let factor = if fast {
                2.
            } else if slow {
                0.25
            } else {
                1.
            };
            for axis in 0..2 {
                let scaled = delta[axis] as f64 * factor + self.fraction[axis];
                let integer = scaled.trunc().clamp(i32::MIN as f64, i32::MAX as f64);
                result[axis] = integer as i32;
                self.fraction[axis] = scaled - integer;
            }
        }
        result
    }
}
fn classify(
    keys: &BTreeSet<u16>,
    relative: &BTreeSet<u16>,
    absolute: &BTreeSet<u16>,
) -> (bool, bool) {
    // Cornechon advertises ABS_VOLUME; that does not make it a touchpad/tablet.
    if [0, 1, 53, 54].iter().any(|axis| absolute.contains(axis))
        || [320, 325, 330].iter().any(|key| keys.contains(key))
    {
        return (false, false);
    }
    (
        relative.contains(&0) && relative.contains(&1),
        keys.iter().any(|key| key_name(*key).is_some()),
    )
}
fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL only access this valid open descriptor's flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
type Identity = (u64, u64, u64);
struct Device {
    source: RawDevice,
    output: VirtualDevice,
    identity: Identity,
    mouse: bool,
    keyboard: bool,
    grabbed: bool,
    held: BTreeSet<u16>,
    forwarded: BTreeSet<u16>,
    delta: [i64; 2],
    packet: Vec<InputEvent>,
}
impl Device {
    fn prepare(
        path: &PathBuf,
        identity: Identity,
        config: &DiscoveryConfig,
    ) -> io::Result<Option<Self>> {
        let source = RawDevice::open(path)?;
        let name = source.name().unwrap_or("input");
        if let Some(prefix) = &config.only_prefix {
            if !name.starts_with(prefix) {
                return Ok(None);
            }
        } else if name.starts_with("mouse-modes:") || name.starts_with("mouse-modes-test") {
            return Ok(None);
        }
        let keys = source
            .supported_keys()
            .map(|set| set.iter().map(|key| key.0).collect())
            .unwrap_or_default();
        let relative = source
            .supported_relative_axes()
            .map(|set| set.iter().map(|axis| axis.0).collect())
            .unwrap_or_default();
        let absolute = source
            .supported_absolute_axes()
            .map(|set| set.iter().map(|axis| axis.0).collect())
            .unwrap_or_default();
        let (mouse, keyboard) = classify(&keys, &relative, &absolute);
        if !mouse && !keyboard {
            return Ok(None);
        }
        // This optional delay exists only in test configuration, not helper IPC.
        if !config.prepare_delay.is_zero() {
            thread::sleep(config.prepare_delay);
        }
        let output_name = format!("mouse-modes: {name}");
        let mut builder = VirtualDevice::builder()?
            .name(&output_name)
            .input_id(source.input_id());
        if let Some(keys) = source.supported_keys() {
            builder = builder.with_keys(keys)?;
        }
        let mut relative: AttributeSet<Rel> = relative.into_iter().map(Rel).collect();
        if mouse {
            for axis in [
                Rel::REL_WHEEL,
                Rel::REL_HWHEEL,
                Rel::REL_WHEEL_HI_RES,
                Rel::REL_HWHEEL_HI_RES,
            ] {
                relative.insert(axis);
            }
        }
        if relative.iter().next().is_some() {
            builder = builder.with_relative_axes(&relative)?;
        }
        if let Some(misc) = source.misc_properties() {
            builder = builder.with_msc(misc)?;
        }
        if source.supported_absolute_axes().is_some() {
            for (axis, info) in source.get_absinfo()? {
                builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
            }
        }
        builder = builder.with_properties(source.properties())?;
        let output = builder.build()?;
        nonblocking(source.as_raw_fd())?;
        Ok(Some(Self {
            source,
            output,
            identity,
            mouse,
            keyboard,
            grabbed: false,
            held: BTreeSet::new(),
            forwarded: BTreeSet::new(),
            delta: [0; 2],
            packet: Vec::with_capacity(16),
        }))
    }
    fn buttons_down(&self) -> bool {
        self.held.iter().any(|key| (0x110..0x120).contains(key))
    }
    fn release(&mut self) -> io::Result<()> {
        self.flush()?;
        let events: Vec<_> = self
            .forwarded
            .iter()
            .map(|key| InputEvent::new(1, *key, 0))
            .collect();
        if !events.is_empty() {
            self.output.emit(&events)?;
        }
        self.forwarded.clear();
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        if !self.packet.is_empty() {
            self.output.emit(&self.packet)?;
            self.packet.clear();
        }
        Ok(())
    }
    fn synchronize_idle(&mut self) -> io::Result<()> {
        // Bounded drain discards history that already reached the desktop.
        for _ in 0..16 {
            match self.source.fetch_events() {
                Ok(events) => {
                    events.for_each(drop);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        self.held = self
            .source
            .get_key_state()?
            .iter()
            .map(|key| key.0)
            .collect();
        Ok(())
    }
    fn shutdown(&mut self) {
        let _ = self.release();
        if self.grabbed {
            let _ = self.source.ungrab();
            self.grabbed = false;
        }
    }
}
#[derive(Clone, Default)]
struct DiscoveryConfig {
    only_prefix: Option<String>,
    prepare_delay: Duration,
}
struct Discovery {
    identities: HashMap<PathBuf, Identity>,
    prepared: Vec<(PathBuf, Device)>,
}
fn discovery_worker(
    config: DiscoveryConfig,
    tx: mpsc::Sender<Discovery>,
    retire: mpsc::Receiver<Device>,
    stop: Arc<AtomicBool>,
) {
    let mut known = HashMap::new();
    while !stop.load(Ordering::Acquire) {
        // Device/UInput destruction can wait for kernel/udev work: never drop a
        // virtual device in the motion-processing thread.
        while let Ok(device) = retire.try_recv() {
            drop(device);
        }
        let mut identities = HashMap::new();
        let mut prepared = Vec::new();
        if let Ok(entries) = fs::read_dir("/dev/input") {
            for entry in entries.flatten() {
                let path = entry.path();
                if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
                    continue;
                }
                let Ok(meta) = fs::metadata(&path) else {
                    continue;
                };
                let identity = (meta.dev(), meta.ino(), meta.rdev());
                identities.insert(path.clone(), identity);
                if known.get(&path) == Some(&identity) {
                    continue;
                }
                match Device::prepare(&path, identity, &config) {
                    Ok(Some(device)) => prepared.push((path, device)),
                    Ok(None) => {}
                    Err(_) => {
                        identities.remove(&path);
                    } // hot-unplug race; retry
                }
            }
        }
        known = identities.clone();
        if tx
            .send(Discovery {
                identities,
                prepared,
            })
            .is_err()
        {
            break;
        }
        for _ in 0..50 {
            if stop.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    while let Ok(device) = retire.try_recv() {
        drop(device);
    }
}
#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "lowercase", deny_unknown_fields)]
enum Command {
    Settings { settings: Settings },
    Capture {},
    Cancel {},
    Quit {},
}
struct Relay {
    devices: BTreeMap<PathBuf, Device>,
    settings: Settings,
    modes: Modes,
    capturing: bool,
    capture_requested: bool,
    drain: bool,
    capture_keys: BTreeSet<String>,
    deadline: Instant,
    last_status: Option<(bool, bool, String)>,
    retire: mpsc::Sender<Device>,
}
impl Relay {
    fn new(retire: mpsc::Sender<Device>) -> Self {
        Self {
            devices: BTreeMap::new(),
            settings: Settings::default(),
            modes: Modes::default(),
            capturing: false,
            capture_requested: false,
            drain: false,
            capture_keys: BTreeSet::new(),
            deadline: Instant::now(),
            last_status: None,
            retire,
        }
    }
    fn held(&self) -> BTreeSet<String> {
        self.devices
            .values()
            .flat_map(|device| &device.held)
            .filter_map(|code| key_name(*code).map(str::to_owned))
            .collect()
    }
    fn keys_down(&self) -> bool {
        self.devices
            .values()
            .filter(|device| device.keyboard)
            .any(|device| device.held.iter().any(|key| keyboard_key(*key)))
    }
    fn recompute(&mut self) -> io::Result<()> {
        self.modes
            .update(&self.settings, &self.held(), self.capturing || self.drain);
        self.sync_grabs()
    }
    fn sync_grabs(&mut self) -> io::Result<()> {
        let configured = bindings(&self.settings)
            .iter()
            .any(|binding| binding.is_some());
        for device in self.devices.values_mut() {
            let wanted =
                (device.mouse && configured) || (device.keyboard && (self.capturing || self.drain));
            if wanted == device.grabbed || (device.mouse && device.buttons_down()) {
                continue;
            }
            if wanted {
                device.synchronize_idle()?;
                device.source.grab()?;
                device.grabbed = true;
            } else {
                device.release()?;
                device.source.ungrab()?;
                device.grabbed = false;
            }
            device.delta = [0; 2];
        }
        Ok(())
    }
    fn status(&mut self, emit: &mut dyn FnMut(Value) -> io::Result<()>) -> io::Result<()> {
        let mice = self.devices.values().filter(|device| device.mouse).count();
        let keyboards = self
            .devices
            .values()
            .filter(|device| device.keyboard)
            .count();
        let status = (
            mice > 0 && keyboards > 0,
            self.capturing,
            format!("Linux: мышей — {mice}, клавиатур — {keyboards}"),
        );
        if self.last_status.as_ref() != Some(&status) {
            emit(
                json!({"event":"engine-status", "payload":EngineStatus {available:status.0,capturing:status.1,message:status.2.clone()}}),
            )?;
            self.last_status = Some(status);
        }
        Ok(())
    }
    fn remove(&mut self, path: &PathBuf) {
        if let Some(mut device) = self.devices.remove(path) {
            device.shutdown();
            let _ = self.retire.send(device);
        }
    }
    fn install(&mut self, batch: Discovery) -> io::Result<()> {
        let removed: Vec<_> = self
            .devices
            .iter()
            .filter(|(path, device)| batch.identities.get(*path) != Some(&device.identity))
            .map(|(path, _)| path.clone())
            .collect();
        for path in removed {
            self.remove(&path);
        }
        for (path, mut device) in batch.prepared {
            if self.devices.contains_key(&path) || device.synchronize_idle().is_err() {
                let _ = self.retire.send(device);
                continue;
            }
            self.devices.insert(path, device);
        }
        self.recompute()
    }
    fn start_capture(&mut self, emit: &mut dyn FnMut(Value) -> io::Result<()>) -> io::Result<()> {
        let blocked = self.devices.values().any(|device| {
            device.keyboard && device.mouse && !device.grabbed && device.buttons_down()
        });
        if self.capture_requested && !self.keys_down() && !blocked {
            self.capture_requested = false;
            self.capturing = true;
            self.capture_keys.clear();
            self.deadline = Instant::now() + Duration::from_secs(20);
            self.recompute()?;
            self.status(emit)?; // acknowledgement follows successful exclusive grabs
        }
        Ok(())
    }
    fn command(
        &mut self,
        command: Command,
        emit: &mut dyn FnMut(Value) -> io::Result<()>,
    ) -> io::Result<bool> {
        match command {
            Command::Settings { settings } => {
                settings.validate().map_err(io::Error::other)?;
                self.settings = settings;
                self.recompute()?;
            }
            Command::Capture {} => {
                if !self.capturing && !self.drain {
                    self.capture_requested = true;
                    self.start_capture(emit)?;
                }
            }
            Command::Cancel {} => {
                self.capture_requested = false;
                self.capturing = false;
                self.drain = self.keys_down()
                    && self
                        .devices
                        .values()
                        .any(|device| device.keyboard && device.grabbed);
                self.capture_keys.clear();
                self.recompute()?;
                self.status(emit)?;
            }
            Command::Quit {} => return Ok(false),
        }
        Ok(true)
    }
    fn event(
        &mut self,
        path: &PathBuf,
        event: InputEvent,
        emit: &mut dyn FnMut(Value) -> io::Result<()>,
    ) -> io::Result<()> {
        if event.event_type() == EventType::SYNCHRONIZATION && event.code() == 3 {
            let device = self.devices.get_mut(path).unwrap();
            if device.grabbed {
                return Err(io::Error::other(
                    "Переполнение очереди ввода. Перезапустите программу.",
                ));
            }
            device.held = device
                .source
                .get_key_state()?
                .iter()
                .map(|key| key.0)
                .collect();
            return self.recompute();
        }
        if event.event_type() == EventType::KEY {
            let device = self.devices.get_mut(path).unwrap();
            if event.value() == 0 {
                device.held.remove(&event.code());
            } else if event.value() == 1 {
                device.held.insert(event.code());
            }
            if device.keyboard && keyboard_key(event.code()) && (self.capturing || self.drain) {
                if self.capturing {
                    self.capture_keys.extend(self.held());
                    if !self.keys_down() && !self.capture_keys.is_empty() {
                        self.capturing = false;
                        let hotkey = normalize(std::mem::take(&mut self.capture_keys));
                        emit(json!({"event":"hotkey-captured","payload":hotkey}))?;
                        self.status(emit)?;
                    }
                }
                if self.drain && !self.keys_down() {
                    self.drain = false;
                }
                return self.recompute();
            }
            self.modes
                .update(&self.settings, &self.held(), self.capturing || self.drain);
            let device = self.devices.get_mut(path).unwrap();
            if device.grabbed {
                let forward = match event.value() {
                    0 => device.forwarded.remove(&event.code()),
                    1 => {
                        device.forwarded.insert(event.code());
                        true
                    }
                    2 => device.forwarded.contains(&event.code()),
                    _ => false,
                };
                if forward {
                    device.packet.push(event);
                }
            }
            self.start_capture(emit)?;
            return self.sync_grabs();
        }
        let device = self.devices.get_mut(path).unwrap();
        if !device.grabbed {
            return Ok(());
        }
        if device.keyboard
            && (self.capturing || self.drain)
            && event.event_type() == EventType::MISC
        {
            return Ok(());
        }
        if device.mouse && event.event_type() == EventType::RELATIVE && event.code() <= 1 {
            device.delta[event.code() as usize] += event.value() as i64;
        } else if event.event_type() == EventType::SYNCHRONIZATION && event.code() == 0 {
            if device.mouse {
                let [x, y, h, v] = self
                    .modes
                    .motion(std::mem::replace(&mut device.delta, [0; 2]));
                for (axis, value) in [(0, x), (1, y), (6, h), (8, v), (12, h * 120), (11, v * 120)]
                {
                    if value != 0 {
                        device.packet.push(InputEvent::new(2, axis, value));
                    }
                }
            }
            device.flush()?;
        } else if [EventType::RELATIVE, EventType::MISC, EventType::ABSOLUTE]
            .contains(&event.event_type())
        {
            device.packet.push(event);
        }
        Ok(())
    }
}
fn run_loop(
    input: RawFd,
    config: DiscoveryConfig,
    emit: &mut dyn FnMut(Value) -> io::Result<()>,
) -> io::Result<()> {
    nonblocking(input)?;
    let (tx, rx) = mpsc::channel();
    let (retire_tx, retire_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || discovery_worker(config, tx, retire_rx, worker_stop));
    let mut relay = Relay::new(retire_tx);
    let result = (|| {
        let mut buffer = Vec::new();
        loop {
            while let Ok(batch) = rx.try_recv() {
                relay.install(batch)?;
                relay.status(emit)?;
            }
            if relay.capturing && Instant::now() >= relay.deadline {
                relay.command(Command::Cancel {}, emit)?;
            }
            let mut paths: Vec<_> = relay.devices.keys().cloned().collect();
            paths.sort_by_key(|path| relay.devices[path].mouse); // keyboards before mice
            let mut pollfds = vec![libc::pollfd {
                fd: input,
                events: libc::POLLIN,
                revents: 0,
            }];
            pollfds.extend(paths.iter().map(|path| libc::pollfd {
                fd: relay.devices[path].source.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
            // SAFETY: pollfds is initialized and valid for its stated length.
            let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, 10) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if pollfds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let mut chunk = [0u8; 4096];
                // SAFETY: chunk is a valid writable byte buffer.
                let count = unsafe { libc::read(input, chunk.as_mut_ptr().cast(), chunk.len()) };
                if count == 0 {
                    return Ok(());
                }
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::WouldBlock
                        && error.kind() != io::ErrorKind::Interrupted
                    {
                        return Err(error);
                    }
                } else {
                    buffer.extend_from_slice(&chunk[..count as usize]);
                    if buffer.len() > 65536 {
                        return Err(io::Error::other("IPC command too large"));
                    }
                    while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<_> = buffer.drain(..=end).collect();
                        let command =
                            serde_json::from_slice::<Command>(&line).map_err(io::Error::other)?;
                        if !relay.command(command, emit)? {
                            return Ok(());
                        }
                    }
                }
            }
            for (index, path) in paths.iter().enumerate() {
                if pollfds[index + 1].revents == 0 || !relay.devices.contains_key(path) {
                    continue;
                }
                let fetched = {
                    let device = relay.devices.get_mut(path).unwrap();
                    device
                        .source
                        .fetch_events()
                        .map(|events| events.collect::<Vec<_>>())
                };
                let events = match fetched {
                    Ok(events) => events,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => {
                        relay.remove(path);
                        relay.recompute()?;
                        relay.status(emit)?;
                        continue;
                    }
                };
                for event in events {
                    relay.event(path, event, emit)?;
                }
            }
        }
    })();
    // Release grabs before waiting for any discovery/UInput destruction work.
    let paths: Vec<_> = relay.devices.keys().cloned().collect();
    for path in paths {
        relay.remove(&path);
    }
    stop.store(true, Ordering::Release);
    // Drop queued prepared devices on their discovery worker, not the relay loop.
    while let Ok(batch) = rx.try_recv() {
        for (_, device) in batch.prepared {
            let _ = relay.retire.send(device);
        }
    }
    drop(rx);
    drop(relay);
    let _ = worker.join();
    result
}
pub fn run_helper() -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut emit = |value: Value| {
        serde_json::to_writer(&mut output, &value)?;
        output.write_all(b"\n")?;
        output.flush()
    };
    let result = (|| -> io::Result<()> {
        // SAFETY: geteuid has no arguments and no memory side effects.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::other(
                "Input helper requires explicit system authorization",
            ));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/run/mouse-modes-app.lock")?;
        // SAFETY: flock operates only on our own lock descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(io::Error::other(
                "Another Mouse Modes input helper is running",
            ));
        }
        run_loop(0, DiscoveryConfig::default(), &mut emit)
    })();
    if let Err(error) = &result {
        let _ = emit(
            json!({"event":"engine-status","payload":EngineStatus {available:false,capturing:false,message:error.to_string()}}),
        );
    }
    result.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn held(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| (*key).into()).collect()
    }
    fn configured() -> Settings {
        let mut s = Settings::default();
        s.hotkeys.slow = Some(Hotkey {
            keys: vec!["F13".into()],
        });
        s.hotkeys.fast = Some(Hotkey {
            keys: vec!["F14".into()],
        });
        s.hotkeys.scroll_horizontal = Some(Hotkey {
            keys: vec!["F15".into()],
        });
        s.hotkeys.scroll_vertical = Some(Hotkey {
            keys: vec!["F16".into()],
        });
        s.hotkeys.scroll_both = Some(Hotkey {
            keys: vec!["F17".into()],
        });
        s
    }
    #[test]
    fn defaults_and_live_settings_while_held() {
        let mut m = Modes::default();
        let keys = held(&["F13"]);
        m.update(&Settings::default(), &keys, false);
        assert_eq!(m.motion([20, -20]), [20, -20, 0, 0]);
        m.update(&configured(), &keys, false);
        assert_eq!(m.motion([20, -20]), [5, -5, 0, 0]);
    }
    #[test]
    fn fast_priority_release_and_fraction_reset() {
        let mut m = Modes::default();
        let s = configured();
        m.update(&s, &held(&["F13", "F14"]), false);
        assert_eq!(m.motion([3, -3]), [6, -6, 0, 0]);
        m.update(&s, &held(&["F13"]), false);
        assert_eq!(m.motion([3, -3]), [0, 0, 0, 0]);
        assert_eq!(m.motion([1, -1]), [1, -1, 0, 0]);
        m.motion([3, 3]);
        m.update(&s, &held(&[]), false);
        assert_eq!(m.motion([1, 1]), [1, 1, 0, 0]);
    }
    #[test]
    fn all_scroll_axes_freeze_pointer_and_preserve_direction() {
        let s = configured();
        let mut m = Modes::default();
        for (keys, expected) in [
            (vec!["F15"], [0, 0, -2, 0]),
            (vec!["F16"], [0, 0, 0, -3]),
            (vec!["F17"], [0, 0, -2, -3]),
            (vec!["F15", "F16"], [0, 0, -2, -3]),
        ] {
            m.update(&s, &held(&keys), false);
            assert_eq!(m.motion([-40, 60]), expected);
        }
    }
    #[test]
    fn scroll_cap_remainder_reset_and_capture_disables_modes() {
        let s = configured();
        let mut m = Modes::default();
        m.update(&s, &held(&["F17"]), false);
        assert_eq!(m.motion([1000, -1000]), [0, 0, 20, 20]);
        assert_eq!(m.motion([0, 0]), [0, 0, 20, 20]);
        m.update(&s, &held(&[]), false);
        m.update(&s, &held(&["F17"]), false);
        assert_eq!(m.motion([0, 0]), [0, 0, 0, 0]);
        m.update(&s, &held(&["F17"]), true);
        assert_eq!(m.motion([4, 4]), [4, 4, 0, 0]);
    }
    #[test]
    fn modifier_sides_any_and_both_side_normalization() {
        let left = Some(Hotkey {
            keys: vec!["ControlLeft".into(), "KeyA".into()],
        });
        assert!(!matches(&left, &held(&["ControlRight", "KeyA"])));
        let any = Some(Hotkey {
            keys: vec!["Control".into(), "KeyA".into()],
        });
        for side in ["ControlLeft", "ControlRight"] {
            assert!(matches(&any, &held(&[side, "KeyA", "ShiftRight"])));
        }
        assert_eq!(
            normalize(held(&["ControlLeft", "ControlRight", "AltLeft", "KeyA"])).keys,
            vec!["AltLeft", "Control", "KeyA"]
        );
    }
    #[test]
    fn cornechon_volume_is_keyboard_but_tablets_are_excluded() {
        let keys = BTreeSet::from([30, 183]);
        assert_eq!(
            classify(&keys, &BTreeSet::from([6, 12]), &BTreeSet::from([32])),
            (false, true)
        );
        assert_eq!(
            classify(&keys, &BTreeSet::from([0, 1]), &BTreeSet::from([32])),
            (true, true)
        );
        for axes in [[0, 1], [53, 54]] {
            assert_eq!(
                classify(&keys, &BTreeSet::new(), &BTreeSet::from(axes)),
                (false, false)
            );
        }
        assert_eq!(
            classify(
                &BTreeSet::from([30, 330]),
                &BTreeSet::new(),
                &BTreeSet::new()
            ),
            (false, false)
        );
    }
    #[test]
    fn mapping_covers_f13_to_f24_and_supported_dom_keys() {
        for number in 13..=24 {
            assert_eq!(
                key_name(183 + number - 13),
                Some(format!("F{number}").as_str())
            );
        }
        for (_, name) in KEY_NAMES {
            assert!(crate::engine::supported_key(name), "{name}");
        }
        assert!(keyboard_key(183));
        assert!(keyboard_key(352));
        assert!(!keyboard_key(272));
    }
    #[test]
    fn protocol_rejects_exec_paths_and_extra_fields() {
        for value in [
            json!({"command":"exec","path":"anything"}),
            json!({"command":"capture","path":"/dev/input/event0"}),
        ] {
            assert!(serde_json::from_value::<Command>(value).is_err());
        }
    }
}

#[cfg(test)]
#[path = "linux_native_tests.rs"]
mod native_tests;
