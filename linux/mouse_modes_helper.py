#!/usr/bin/python3
"""Private stdio-controlled evdev relay. EOF releases every device immediately.

Run with /usr/bin/python3 -I. Only JSON settings/capture commands are accepted;
no user paths or executable commands are part of the protocol.
"""
import fcntl
import json
import math
import os
import selectors
import signal
import sys
import time
import queue
import threading

ACTIONS = ('slow', 'fast', 'scrollHorizontal', 'scrollVertical', 'scrollBoth')
MODIFIERS = ('Control', 'Shift', 'Alt', 'Meta')


def normalize(keys):
    keys = set(keys)
    for modifier in MODIFIERS:
        pair = {modifier + 'Left', modifier + 'Right'}
        if pair <= keys:
            keys -= pair
            keys.add(modifier)
    return sorted(keys)


def matches(binding, held):
    return bool(binding) and all(
        bool({key + 'Left', key + 'Right'} & held) if key in MODIFIERS else key in held
        for key in binding['keys'])


class Modes:
    def __init__(self):
        self.bindings = dict.fromkeys(ACTIONS)
        self.state = (False,) * 5
        self.fraction = [0., 0.]
        self.scroll = [0., 0.]

    def update(self, held, disabled=False):
        state = tuple(matches(self.bindings[name], held) and not disabled for name in ACTIONS)
        if self.state != state:
            self.fraction = [0., 0.]
            self.scroll = [0., 0.]
        self.state = state

    def motion(self, dx, dy):
        slow, fast, horizontal, vertical, both = self.state
        if horizontal or vertical or both:
            wheels = [0, 0]
            for axis, active, delta in ((0, horizontal or both, dx), (1, vertical or both, dy)):
                if active:
                    self.scroll[axis] += delta
                    steps = max(-20, min(20, math.trunc(self.scroll[axis] / 20)))
                    self.scroll[axis] -= steps * 20
                    wheels[axis] = steps
                else:
                    self.scroll[axis] = 0
            return 0, 0, wheels[0], -wheels[1]
        factor = 2. if fast else .25 if slow else 1.
        result = []
        for axis, delta in enumerate((dx, dy)):
            value = delta * factor + self.fraction[axis]
            whole = math.trunc(value)
            self.fraction[axis] = value - whole
            result.append(whole)
        return *result, 0, 0


def code_map(e):
    names = {'LEFTCTRL': 'ControlLeft', 'RIGHTCTRL': 'ControlRight',
             'LEFTSHIFT': 'ShiftLeft', 'RIGHTSHIFT': 'ShiftRight',
             'LEFTALT': 'AltLeft', 'RIGHTALT': 'AltRight',
             'LEFTMETA': 'MetaLeft', 'RIGHTMETA': 'MetaRight',
             'ESC': 'Escape', 'ENTER': 'Enter', 'BACKSPACE': 'Backspace',
             'TAB': 'Tab', 'SPACE': 'Space', 'CAPSLOCK': 'CapsLock',
             'UP': 'ArrowUp', 'DOWN': 'ArrowDown', 'LEFT': 'ArrowLeft', 'RIGHT': 'ArrowRight',
             'PAGEUP': 'PageUp', 'PAGEDOWN': 'PageDown', 'HOME': 'Home', 'END': 'End',
             'INSERT': 'Insert', 'DELETE': 'Delete', 'NUMLOCK': 'NumLock',
             'SCROLLLOCK': 'ScrollLock', 'SYSRQ': 'PrintScreen', 'PAUSE': 'Pause',
             'MINUS': 'Minus', 'EQUAL': 'Equal', 'LEFTBRACE': 'BracketLeft',
             'RIGHTBRACE': 'BracketRight', 'BACKSLASH': 'Backslash', 'SEMICOLON': 'Semicolon',
             'APOSTROPHE': 'Quote', 'GRAVE': 'Backquote', 'COMMA': 'Comma',
             'DOT': 'Period', 'SLASH': 'Slash', 'COMPOSE': 'ContextMenu',
             'KPENTER': 'NumpadEnter', 'KPPLUS': 'NumpadAdd', 'KPMINUS': 'NumpadSubtract',
             'KPASTERISK': 'NumpadMultiply', 'KPSLASH': 'NumpadDivide', 'KPDOT': 'NumpadDecimal'}
    names.update({chr(n): 'Key' + chr(n) for n in range(65, 91)})
    names.update({str(n): 'Digit' + str(n) for n in range(10)})
    names.update({'KP' + str(n): 'Numpad' + str(n) for n in range(10)})
    names.update({'F' + str(n): 'F' + str(n) for n in range(1, 25)})
    return {getattr(e, 'KEY_' + name): dom for name, dom in names.items() if hasattr(e, 'KEY_' + name)}


def classify_device(caps, names, e):
    """A keyboard's ABS_VOLUME is not an absolute pointing device.

    Some ZMK composite HID keyboards advertise a volume absolute axis alongside
    ordinary keys/wheels. Reject only actual absolute/touch pointer capabilities,
    while allowing non-pointer axes to retain normal consumer-control behavior.
    """
    keys = set(caps.get(e.EV_KEY, []))
    absolute = {axis[0] for axis in caps.get(e.EV_ABS, [])}
    pointer_axes = {e.ABS_X, e.ABS_Y, e.ABS_MT_POSITION_X, e.ABS_MT_POSITION_Y}
    touch_buttons = {e.BTN_TOUCH, e.BTN_TOOL_FINGER, e.BTN_TOOL_PEN}
    if absolute & pointer_axes or keys & touch_buttons:
        return False, False
    mouse = {e.REL_X, e.REL_Y} <= set(caps.get(e.EV_REL, []))
    keyboard = bool(keys & names.keys())
    return mouse, keyboard


def emit(event, payload):
    print(json.dumps({'event': event, 'payload': payload}), flush=True)


class Relay:
    def __init__(self):
        from evdev import ecodes
        self.e = ecodes
        self.names = code_map(ecodes)
        self.selector = selectors.DefaultSelector()
        self.devices = {}
        self.modes = Modes()
        self.capturing = False
        self.capture_keys = set()
        self.capture_blocked = False
        self.capture_deadline = 0
        self.drain = False
        self.capture_requested = False
        self.discovered = queue.SimpleQueue()
        self.retired = queue.SimpleQueue()
        self.discovery_stop = threading.Event()
        self.last_status = None

    def held(self):
        return {self.names[code] for device in self.devices.values() for code in device['held'] if code in self.names}

    def keys_down(self):
        # Include unsupported consumer keys in the drain barrier, while excluding
        # mouse buttons on devices exposing both keyboard and mouse capabilities.
        return any(code < 0x100 or code >= 0x160
                   for device in self.devices.values() if device['keyboard']
                   for code in device['held'])

    def recompute(self):
        self.modes.update(self.held(), self.capturing or self.drain)
        self.sync_grabs()

    def sync_grabs(self):
        active = any(self.modes.bindings.values())
        for device in self.devices.values():
            wanted = ((device['mouse'] and active) or
                      (device['keyboard'] and (self.capturing or self.drain)))
            if wanted == device['grabbed']:
                continue
            # Grabs change only when bindings become empty/non-empty or while
            # recording. Never transfer button ownership mid-drag on those
            # settings transitions. Hotkey presses never switch mouse ownership.
            buttons = {code for code in device['held'] if 0x110 <= code < 0x120}
            if device['mouse'] and buttons:
                continue
            if wanted:
                device['source'].grab()
                device['grabbed'] = True
                device['motion'] = [0, 0]
            else:
                self.release(device)
                device['source'].ungrab()
                device['grabbed'] = False
                device['motion'] = [0, 0]

    def status(self):
        mice = sum(device['mouse'] for device in self.devices.values())
        keyboards = sum(device['keyboard'] for device in self.devices.values())
        payload = {'available': bool(mice and keyboards), 'capturing': self.capturing,
                   'message': f'Linux: мышей — {mice}, клавиатур — {keyboards}'}
        if payload != self.last_status:
            self.last_status = payload
            emit('engine-status', payload)

    def release(self, device):
        for code in device['forwarded']:
            device['virtual'].write(self.e.EV_KEY, code, 0)
        if device['forwarded']:
            device['virtual'].syn()
        device['forwarded'].clear()

    def remove(self, path):
        device = self.devices.pop(path)
        try:
            self.release(device)
        finally:
            self.selector.unregister(device['source'])
            device['source'].close()
            self.retired.put(device['virtual'])
        self.recompute()

    def discover(self):
        """All slow node probing/uinput discovery runs away from input processing.

        The worker owns prepared handles until the event-loop queue accepts them;
        only the event thread ever grabs devices or edits selector/device state.
        """
        from evdev import InputDevice, UInput, list_devices
        known = {}
        try:
            while not self.discovery_stop.is_set():
                while not self.retired.empty():
                    self.retired.get_nowait().close()
                paths = set(list_devices())
                identities = {}
                prepared = []
                for path in paths:
                    source = virtual = None
                    try:
                        stat = os.stat(path)
                        identity = (stat.st_ino, stat.st_rdev)
                        identities[path] = identity
                        if known.get(path) == identity:
                            continue
                        source = InputDevice(path)
                        virtual = None
                        caps = source.capabilities()
                        mouse, keyboard = classify_device(caps, self.names, self.e)
                        if (source.name.startswith(('mouse-modes:', 'mouse-modes-test')) or
                                not (mouse or keyboard)):
                            source.close()
                            continue
                        relay_caps = {kind: codes for kind, codes in caps.items()
                                      if kind in (self.e.EV_KEY, self.e.EV_REL, self.e.EV_MSC, self.e.EV_ABS)}
                        if mouse:
                            relay_caps[self.e.EV_REL] = sorted(set(relay_caps[self.e.EV_REL]) | {
                                self.e.REL_WHEEL, self.e.REL_HWHEEL,
                                self.e.REL_WHEEL_HI_RES, self.e.REL_HWHEEL_HI_RES})
                        virtual = UInput(relay_caps, name='mouse-modes: ' + source.name,
                                         vendor=source.info.vendor, product=source.info.product,
                                         version=source.info.version, bustype=source.info.bustype,
                                         input_props=source.input_props())
                        prepared.append((path, identity, {
                            'source': source, 'virtual': virtual, 'mouse': mouse,
                            'keyboard': keyboard, 'held': set(), 'forwarded': set(),
                            'motion': [0, 0], 'grabbed': False}))
                    except (OSError, SystemError):
                        # evdev ioctl wrappers can also raise SystemError when a
                        # source disappears during its input_props() query.
                        # Unplug/open races must not stop existing motion relays.
                        if source is not None:
                            source.close()
                        if virtual is not None:
                            virtual.close()
                        identities.pop(path, None)  # retry if it still exists
                known = identities
                self.discovered.put((identities, prepared))
                self.discovery_stop.wait(.5)
        except Exception as error:
            self.discovered.put(error)

    def install_discovery(self):
        while not self.discovered.empty():
            batch = self.discovered.get_nowait()
            if isinstance(batch, Exception):
                raise batch
            identities, prepared = batch
            for path in list(self.devices):
                if identities.get(path) != self.devices[path]['identity']:
                    self.remove(path)
            for path, identity, device in prepared:
                if path in self.devices:
                    device['source'].close()
                    self.retired.put(device['virtual'])
                    continue
                device['identity'] = identity
                # Discard pre-observation history: it already reached the desktop.
                try:
                    for _ in range(16):
                        list(device['source'].read())
                except BlockingIOError:
                    pass
                except OSError:
                    device['source'].close()
                    self.retired.put(device['virtual'])
                    continue
                device['held'] = set(device['source'].active_keys())
                self.devices[path] = device
                self.selector.register(device['source'], selectors.EVENT_READ, path)
            self.recompute()
            self.status()

    def command(self, value):
        command = value.get('command')
        if command == 'settings':
            bindings = value['settings']['hotkeys']
            if set(bindings) != set(ACTIONS):
                raise ValueError('Invalid settings fields')
            valid = set(self.names.values()) | set(MODIFIERS)
            for binding in bindings.values():
                if binding is not None and (not isinstance(binding, dict) or
                       not isinstance(binding.get('keys'), list) or not 1 <= len(binding['keys']) <= 16 or
                       any(not isinstance(key, str) or key not in valid for key in binding['keys'])):
                    raise ValueError('Unsupported hotkey')
            self.modes.bindings = bindings
            self.recompute()
        elif command == 'capture':
            if self.capturing or self.drain:
                self.status()
                return
            self.capture_requested = True
            self.start_capture_if_ready()
        elif command == 'cancel':
            self.capture_requested = False
            self.capturing = False
            self.drain = self.keys_down() and any(d['grabbed'] and d['keyboard'] for d in self.devices.values())
            self.capture_keys.clear()
            self.recompute()
            self.status()
        elif command == 'quit':
            raise EOFError
        else:
            raise ValueError('Unknown command')

    def start_capture_if_ready(self):
        mixed_buttons = any(device['keyboard'] and device['mouse'] and not device['grabbed'] and
                            any(0x110 <= code < 0x120 for code in device['held'])
                            for device in self.devices.values())
        if self.capture_requested and not self.keys_down() and not mixed_buttons:
            self.capture_requested = False
            self.capturing = True
            self.capture_keys.clear()
            self.capture_blocked = self.keys_down()
            self.capture_deadline = time.monotonic() + 20
            for device in self.devices.values():
                if device['keyboard']:
                    self.release(device)
            self.recompute()
            self.status()

    def event(self, device, event):
        e = self.e
        virtual = device['virtual']
        if event.type == e.EV_SYN and event.code == e.SYN_DROPPED:
            if not device['grabbed']:
                device['held'] = set(device['source'].active_keys())
                self.recompute()
                return
            raise RuntimeError('Переполнение очереди ввода. Служба безопасно остановлена; перезапустите программу.')
        if event.type == e.EV_KEY:
            if event.value == 0:
                device['held'].discard(event.code)
            elif event.value == 1:
                device['held'].add(event.code)
            if device['keyboard'] and (self.capturing or self.drain):
                held = self.held()
                if self.capturing:
                    if not self.capture_blocked:
                        self.capture_keys.update(held)
                    if not self.keys_down():
                        if self.capture_blocked:
                            self.capture_blocked = False
                        elif self.capture_keys:
                            self.capturing = False
                            emit('hotkey-captured', {'keys': normalize(self.capture_keys)})
                            self.capture_keys.clear()
                            self.status()
                if self.drain and not self.keys_down():
                    self.drain = False
                self.recompute()
                return
            self.modes.update(self.held(), self.capturing or self.drain)
            if not device['grabbed']:
                self.start_capture_if_ready()
                self.sync_grabs()
                return
            if event.value == 0:
                # Do not send unmatched releases for keys suppressed during capture.
                if event.code not in device['forwarded']:
                    return
                device['forwarded'].discard(event.code)
            elif event.value == 1:
                device['forwarded'].add(event.code)
            elif event.value == 2 and event.code not in device['forwarded']:
                return
            virtual.write_event(event)
            if event.value == 0:
                virtual.syn()
            self.sync_grabs()
        elif not device['grabbed']:
            return
        elif device['keyboard'] and (self.capturing or self.drain) and event.type == e.EV_MSC:
            return
        elif device['mouse'] and event.type == e.EV_REL and event.code in (e.REL_X, e.REL_Y):
            device['motion'][event.code == e.REL_Y] += event.value
        elif event.type == e.EV_SYN and event.code == e.SYN_REPORT:
            if device['mouse']:
                x, y, h, v = self.modes.motion(*device['motion'])
                device['motion'] = [0, 0]
                for code, amount in ((e.REL_X, x), (e.REL_Y, y), (e.REL_HWHEEL, h),
                                     (e.REL_WHEEL, v), (e.REL_HWHEEL_HI_RES, h * 120),
                                     (e.REL_WHEEL_HI_RES, v * 120)):
                    if amount:
                        virtual.write(e.EV_REL, code, amount)
            virtual.syn()
        elif event.type in (e.EV_REL, e.EV_MSC, e.EV_ABS):
            virtual.write_event(event)

    def read_ready(self, path):
        device = self.devices[path]
        try:
            for event in device['source'].read():
                self.event(device, event)
        except BlockingIOError:
            pass  # readiness is advisory, not a disconnected device
        except OSError:
            self.remove(path)

    def run(self):
        self.selector.register(sys.stdin.buffer, selectors.EVENT_READ, None)
        buffer = b''
        worker = threading.Thread(target=self.discover, daemon=True)
        worker.start()
        try:
            while True:
                now = time.monotonic()
                self.install_discovery()
                if self.capturing and now >= self.capture_deadline:
                    self.command({'command': 'cancel'})
                ready = self.selector.select(.01)
                ready.sort(key=lambda item: item[0].data is not None and self.devices[item[0].data]['mouse'])
                for selected, _ in ready:
                    if selected.data is None:
                        chunk = os.read(sys.stdin.fileno(), 4096)
                        if not chunk:
                            return
                        buffer += chunk
                        if len(buffer) > 65536:
                            raise ValueError('IPC command too large')
                        while b'\n' in buffer:
                            line, buffer = buffer.split(b'\n', 1)
                            self.command(json.loads(line))
                    else:
                        self.read_ready(selected.data)
        except EOFError:
            pass
        finally:
            self.discovery_stop.set()
            worker.join(timeout=3)
            while not self.discovered.empty():
                batch = self.discovered.get_nowait()
                if not isinstance(batch, Exception):
                    for _, _, device in batch[1]:
                        device["source"].close()
                        device["virtual"].close()
            for path in list(self.devices):
                self.remove(path)
            while not self.retired.empty():
                self.retired.get_nowait().close()
            self.selector.close()


def main():
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        # Root-owned /run file serializes relay instances; never follow symlinks.
        fd = os.open('/run/mouse-modes-app.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            Relay().run()
    except (Exception, KeyboardInterrupt) as error:
        emit('engine-status', {'available': False, 'capturing': False, 'message': str(error)})
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
