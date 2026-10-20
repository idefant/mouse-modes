//! Root-only opt-in kernel regressions. Physical fixture names use the test
//! prefix ignored by the running application. We copy capabilities, never real
//! user events. Mouse outputs are grabbed before any motion. Standalone test
//! function/modifier keys outside recording are desktop-visible: run unattended.
use super::*;
use evdev::{AbsInfo, AbsoluteAxisCode, KeyCode};
use std::{collections::VecDeque, net::Shutdown, os::unix::net::UnixStream, time::SystemTime};

fn keys(codes: &[u16]) -> AttributeSet<KeyCode> {
    codes.iter().copied().map(KeyCode).collect()
}
fn relative(codes: &[Rel]) -> AttributeSet<Rel> {
    codes.iter().copied().collect()
}
fn node(name: &str) -> io::Result<PathBuf> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        for entry in fs::read_dir("/sys/class/input")?.flatten() {
            if !entry.file_name().as_encoded_bytes().starts_with(b"event") {
                continue;
            }
            if fs::read_to_string(entry.path().join("device/name"))
                .is_ok_and(|text| text.trim_end() == name)
            {
                return Ok(PathBuf::from("/dev/input").join(entry.file_name()));
            }
        }
        if Instant::now() >= end {
            return Err(io::Error::other(format!("Missing fixture {name}")));
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn read(device: &mut RawDevice, duration: Duration) -> Vec<InputEvent> {
    let end = Instant::now() + duration;
    let mut events = Vec::new();
    for _ in 0..256 {
        match device.fetch_events() {
            Ok(packet) => {
                events.extend(packet);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("Synthetic input read: {error}"),
        }
        if Instant::now() >= end {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    events
}
fn keyboard(name: &str, combined: bool) -> io::Result<VirtualDevice> {
    // Copy the actual Cornechon descriptor if connected; no physical fetch_events.
    let corne = fs::read_dir("/sys/class/input")?
        .flatten()
        .find_map(|entry| {
            let name = fs::read_to_string(entry.path().join("device/name")).ok()?;
            if name.trim_end() == "ZMK Project Cornechon Keyboard" {
                RawDevice::open(PathBuf::from("/dev/input").join(entry.file_name())).ok()
            } else {
                None
            }
        });
    if let Some(source) = corne.filter(|_| !combined) {
        let mut b = VirtualDevice::builder()?
            .name(name)
            .input_id(source.input_id());
        if let Some(keys) = source.supported_keys() {
            b = b.with_keys(keys)?;
        }
        if let Some(axes) = source.supported_relative_axes() {
            b = b.with_relative_axes(axes)?;
        }
        if let Some(misc) = source.misc_properties() {
            b = b.with_msc(misc)?;
        }
        for (axis, info) in source.get_absinfo()? {
            b = b.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
        }
        return b.build();
    }
    let axes = if combined {
        relative(&[
            Rel::REL_X,
            Rel::REL_Y,
            Rel::REL_HWHEEL,
            Rel::REL_HWHEEL_HI_RES,
        ])
    } else {
        relative(&[Rel::REL_HWHEEL, Rel::REL_HWHEEL_HI_RES])
    };
    VirtualDevice::builder()?
        .name(name)
        .with_keys(&keys(&[29, 97, 30, 183, 184, 185, 186, 187, 272]))?
        .with_relative_axes(&axes)?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_VOLUME,
            AbsInfo::new(0, 0, 1023, 0, 0, 0),
        ))?
        .build()
}
struct Fixture {
    control: UnixStream,
    events: mpsc::Receiver<Value>,
    worker: Option<thread::JoinHandle<io::Result<()>>>,
    mouse: VirtualDevice,
    keyboard: VirtualDevice,
    mouse_source: RawDevice,
    keyboard_source: RawDevice,
    mouse_output: RawDevice,
    keyboard_output: RawDevice,
    prefix: String,
}
impl Fixture {
    fn new(delay: Duration) -> Self {
        Self::new_with(delay, false)
    }
    fn new_with(delay: Duration, combined: bool) -> Self {
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "Ignored native tests require administrator privileges"
        );
        let prefix = format!(
            "mouse-modes-test-native-{}-{}-",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let mouse_name = format!("{prefix}mouse");
        let key_name = format!("{prefix}keyboard");
        let mouse = VirtualDevice::builder()
            .unwrap()
            .name(&mouse_name)
            .with_keys(&keys(&[272]))
            .unwrap()
            .with_relative_axes(&relative(&[Rel::REL_X, Rel::REL_Y]))
            .unwrap()
            .build()
            .unwrap();
        let keyboard = keyboard(&key_name, combined).unwrap();
        let (control, helper) = UnixStream::pair().unwrap();
        let (tx, events) = mpsc::channel();
        let config = DiscoveryConfig {
            only_prefix: Some(prefix.clone()),
            prepare_delay: delay,
        };
        let worker = thread::spawn(move || {
            run_loop(helper.as_raw_fd(), config, &mut |event| {
                tx.send(event).map_err(io::Error::other)
            })
        });
        let mouse_source = RawDevice::open(node(&mouse_name).unwrap()).unwrap();
        let keyboard_source = RawDevice::open(node(&key_name).unwrap()).unwrap();
        nonblocking(mouse_source.as_raw_fd()).unwrap();
        nonblocking(keyboard_source.as_raw_fd()).unwrap();
        loop {
            let event = events.recv_timeout(Duration::from_secs(15)).unwrap();
            if event["payload"]["available"] == true {
                break;
            }
        }
        let mut mouse_output =
            RawDevice::open(node(&format!("mouse-modes: {mouse_name}")).unwrap()).unwrap();
        let mut keyboard_output =
            RawDevice::open(node(&format!("mouse-modes: {key_name}")).unwrap()).unwrap();
        nonblocking(mouse_output.as_raw_fd()).unwrap();
        nonblocking(keyboard_output.as_raw_fd()).unwrap();
        mouse_output.grab().unwrap();
        keyboard_output.grab().unwrap();
        Self {
            control,
            events,
            worker: Some(worker),
            mouse,
            keyboard,
            mouse_source,
            keyboard_source,
            mouse_output,
            keyboard_output,
            prefix,
        }
    }
    fn command(&mut self, value: Value) {
        writeln!(self.control, "{value}").unwrap();
    }
    fn event(&mut self, name: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let event = self
                .events
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .unwrap();
            if event["event"] == name && predicate(&event["payload"]) {
                return event["payload"].clone();
            }
        }
    }
    fn capturing(&mut self, value: bool) {
        self.event("engine-status", |payload| payload["capturing"] == value);
    }
    fn key(&mut self, code: u16, value: i32) {
        self.keyboard
            .emit(&[InputEvent::new(1, code, value)])
            .unwrap();
        thread::sleep(Duration::from_millis(15));
    }
    fn settings(&mut self, settings: Settings) {
        self.command(json!({"command":"settings","settings":settings}));
        thread::sleep(Duration::from_millis(30));
    }
    fn motion(&mut self, x: i32, y: i32) -> [i32; 4] {
        read(&mut self.mouse_output, Duration::ZERO);
        self.mouse
            .emit(&[InputEvent::new(2, 0, x), InputEvent::new(2, 1, y)])
            .unwrap();
        let mut values = [0; 4];
        for e in read(&mut self.mouse_output, Duration::from_millis(20)) {
            if e.event_type() == EventType::RELATIVE {
                if let Some(axis) = [0, 1, 6, 8].iter().position(|code| *code == e.code()) {
                    values[axis] += e.value();
                }
            }
        }
        values
    }
    fn begin(&mut self) {
        self.command(json!({"command":"capture"}));
        self.capturing(true);
    }
    fn captured(&mut self) -> Hotkey {
        let value = self.event("hotkey-captured", |_| true);
        self.capturing(false);
        serde_json::from_value(value).unwrap()
    }
    fn finish(&mut self) {
        self.control.shutdown(Shutdown::Both).unwrap();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap().unwrap();
        }
        self.mouse_source.grab().unwrap();
        self.mouse_source.ungrab().unwrap();
        self.keyboard_source.grab().unwrap();
        self.keyboard_source.ungrab().unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.control.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
#[ignore = "root-only isolated evdev/uinput parity; run unattended"]
fn native_parity() {
    let mut f = Fixture::new(Duration::ZERO);
    assert!(f
        .keyboard_output
        .supported_absolute_axes()
        .unwrap()
        .contains(AbsoluteAxisCode::ABS_VOLUME));
    f.mouse_source.grab().unwrap();
    f.mouse_source.ungrab().unwrap();
    f.keyboard_source.grab().unwrap();
    f.keyboard_source.ungrab().unwrap();
    f.key(183, 1);
    f.settings(configured());
    assert_eq!(f.motion(20, -20), [5, -5, 0, 0]);
    f.key(184, 1);
    assert_eq!(f.motion(20, -20), [40, -40, 0, 0]);
    f.key(184, 0);
    f.key(183, 0);
    for (code, expected) in [
        (185, [0, 0, 2, 0]),
        (186, [0, 0, 0, -3]),
        (187, [0, 0, 2, -3]),
    ] {
        f.key(code, 1);
        assert_eq!(f.motion(40, 60), expected);
        f.key(code, 0);
    }
    f.key(185, 1);
    f.key(186, 1);
    assert_eq!(f.motion(40, 60), [0, 0, 2, -3]);
    f.key(185, 0);
    f.key(186, 0);
    read(&mut f.mouse_output, Duration::ZERO);
    f.mouse.emit(&[InputEvent::new(1, 272, 1)]).unwrap();
    assert_eq!(
        read(&mut f.mouse_output, Duration::from_millis(20))
            .iter()
            .filter(|e| e.event_type() == EventType::KEY)
            .map(InputEvent::value)
            .collect::<Vec<_>>(),
        vec![1]
    );
    f.key(183, 1);
    assert_eq!(f.motion(20, 0), [5, 0, 0, 0]);
    f.key(183, 0);
    f.mouse.emit(&[InputEvent::new(1, 272, 0)]).unwrap();
    assert_eq!(
        read(&mut f.mouse_output, Duration::from_millis(20))
            .iter()
            .filter(|e| e.event_type() == EventType::KEY)
            .map(InputEvent::value)
            .collect::<Vec<_>>(),
        vec![0]
    );
    let mut s = configured();
    s.hotkeys.slow = Some(Hotkey {
        keys: vec!["ControlLeft".into()],
    });
    f.settings(s.clone());
    f.key(97, 1);
    assert_eq!(f.motion(20, 0), [20, 0, 0, 0]);
    f.key(97, 0);
    f.key(29, 1);
    assert_eq!(f.motion(20, 0), [5, 0, 0, 0]);
    f.key(29, 0);
    s.hotkeys.slow = Some(Hotkey {
        keys: vec!["Control".into()],
    });
    f.settings(s);
    for code in [29, 97] {
        f.key(code, 1);
        assert_eq!(f.motion(20, 0), [5, 0, 0, 0]);
        f.key(code, 0);
    }
    for n in 13..=16 {
        read(&mut f.keyboard_source, Duration::ZERO);
        read(&mut f.keyboard_output, Duration::ZERO);
        f.begin();
        f.key(183 + n - 13, 1);
        f.key(183 + n - 13, 2);
        f.key(183 + n - 13, 0);
        assert_eq!(f.captured().keys, vec![format!("F{n}")]);
        assert!(!read(&mut f.keyboard_source, Duration::ZERO)
            .iter()
            .any(|e| e.event_type() == EventType::KEY));
        assert!(!read(&mut f.keyboard_output, Duration::ZERO)
            .iter()
            .any(|e| e.event_type() == EventType::KEY));
    }
    f.begin();
    for (code, value) in [(29, 1), (97, 1), (30, 1), (29, 0), (30, 0), (97, 0)] {
        f.key(code, value);
    }
    assert_eq!(f.captured().keys, vec!["Control", "KeyA"]);
    f.begin();
    f.key(30, 1);
    f.command(json!({"command":"cancel"}));
    f.capturing(false);
    f.key(30, 2);
    f.key(30, 0);
    assert!(!read(&mut f.keyboard_output, Duration::ZERO)
        .iter()
        .any(|e| e.event_type() == EventType::KEY));
    f.keyboard_source.grab().unwrap();
    f.keyboard_source.ungrab().unwrap();
    f.key(29, 1);
    f.command(json!({"command":"capture"}));
    thread::sleep(Duration::from_millis(20));
    f.key(29, 0);
    f.capturing(true);
    f.key(183, 1);
    f.key(183, 0);
    assert_eq!(f.captured().keys, vec!["F13"]);
    f.settings(Settings::default());
    f.mouse_source.grab().unwrap();
    f.mouse_source.ungrab().unwrap();
    f.finish();
    println!("native_parity: Cornechon ABS_VOLUME, five modes, live settings/drag, sides/ANY, F13-F16/repeats/cancel/no-leak/preheld/EOF PASS");
}
#[test]
#[ignore = "root-only composite keyboard/mouse capture regression"]
fn native_composite_capture_buttons() {
    let mut f = Fixture::new_with(Duration::ZERO, true);
    f.begin();
    f.keyboard
        .emit(&[InputEvent::new(1, 272, 1), InputEvent::new(2, 0, 7)])
        .unwrap();
    let events = read(&mut f.keyboard_output, Duration::from_millis(20));
    assert!(events
        .iter()
        .any(|e| e.event_type() == EventType::KEY && e.code() == 272 && e.value() == 1));
    assert!(events
        .iter()
        .any(|e| e.event_type() == EventType::RELATIVE && e.code() == 0 && e.value() == 7));
    f.key(183, 1);
    f.key(183, 0);
    assert_eq!(f.captured().keys, vec!["F13"]);
    f.keyboard.emit(&[InputEvent::new(1, 272, 0)]).unwrap();
    let events = read(&mut f.keyboard_output, Duration::from_millis(20));
    assert!(events
        .iter()
        .any(|e| e.event_type() == EventType::KEY && e.code() == 272 && e.value() == 0));
    assert!(!events
        .iter()
        .any(|e| e.event_type() == EventType::KEY && e.code() == 183));
    f.finish();
}
#[test]
#[ignore = "root-only isolated 1kHz relay plus 1.9s discovery and hotplug"]
fn native_latency() {
    let mut f = Fixture::new(Duration::from_millis(1900));
    f.settings(configured());
    for (mode, key) in [
        ("idle", None),
        ("slow", Some(183)),
        ("fast", Some(184)),
        ("scroll", Some(187)),
    ] {
        if let Some(code) = key {
            f.key(code, 1);
        }
        read(&mut f.mouse_output, Duration::ZERO);
        let output = &mut f.mouse_output;
        let mouse = &mut f.mouse;
        let prefix = f.prefix.clone();
        let (tx, rx) = mpsc::channel();
        let count = 4500;
        let mut latency = Vec::new();
        thread::scope(|scope| {
            scope.spawn(move || {
                let start = Instant::now();
                for index in 0..count {
                    thread::sleep(
                        (start + Duration::from_micros(index * 1000))
                            .saturating_duration_since(Instant::now()),
                    );
                    tx.send(SystemTime::now()).unwrap();
                    mouse
                        .emit(&[InputEvent::new(2, 0, 20), InputEvent::new(2, 1, 20)])
                        .unwrap();
                }
            });
            scope.spawn(move || {
                thread::sleep(Duration::from_millis(700));
                let name = format!("{prefix}hotplug-{mode}");
                let extra = VirtualDevice::builder()
                    .unwrap()
                    .name(&name)
                    .with_relative_axes(&relative(&[Rel::REL_X, Rel::REL_Y]))
                    .unwrap()
                    .build()
                    .unwrap();
                thread::sleep(Duration::from_millis(2800));
                drop(extra);
            });
            let end = Instant::now() + Duration::from_secs(8);
            let mut pending = VecDeque::new();
            while latency.len() < count as usize && Instant::now() < end {
                pending.extend(rx.try_iter());
                for event in read(output, Duration::ZERO) {
                    if event.event_type() == EventType::SYNCHRONIZATION && event.code() == 0 {
                        let sent = pending
                            .pop_front()
                            .or_else(|| rx.recv_timeout(Duration::from_secs(1)).ok())
                            .unwrap();
                        latency.push(
                            event
                                .timestamp()
                                .duration_since(sent)
                                .unwrap_or_default()
                                .as_secs_f64()
                                * 1000.,
                        );
                    }
                }
                thread::sleep(Duration::from_micros(100));
            }
        });
        assert_eq!(latency.len(), count as usize, "{mode}: missing packets");
        latency.sort_by(f64::total_cmp);
        let p95 = latency[4275];
        let p99 = latency[4455];
        let max = latency[4499];
        println!("native_latency {mode}: packets={count}, p95={p95:.3}ms, p99={p99:.3}ms, max={max:.3}ms, discovery_delay=1900ms");
        assert!(p99 < 40. && max < 100., "{mode}: blocking relay");
        if let Some(code) = key {
            f.key(code, 0);
        }
    }
    f.finish();
}
