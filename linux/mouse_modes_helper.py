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

    def status(self):
        mice = sum(device['mouse'] for device in self.devices.values())
        keyboards = sum(device['keyboard'] for device in self.devices.values())
        emit('engine-status', {'available': bool(mice and keyboards), 'capturing': self.capturing,
                              'message': f'Linux: мышей — {mice}, клавиатур — {keyboards}'})

    def release(self, device):
        for code in device['forwarded']:
            device['virtual'].write(self.e.EV_KEY, code, 0)
        device['virtual'].syn()
        device['forwarded'].clear()

    def remove(self, path):
        device = self.devices.pop(path)
        try:
            self.release(device)
        finally:
            self.selector.unregister(device['source'])
            device['source'].close()  # closes EVIOCGRAB even if ungrab fails
            device['virtual'].close()
        self.recompute()

    def scan(self):
        from evdev import InputDevice, UInput, list_devices
        paths = set(list_devices())
        for path in list(self.devices):
            if path not in paths:
                self.remove(path)
        for path in paths - self.devices.keys():
            source = InputDevice(path)
            virtual = None
            try:
                caps = source.capabilities()
                keys = set(caps.get(self.e.EV_KEY, []))
                mouse = {self.e.REL_X, self.e.REL_Y} <= set(caps.get(self.e.EV_REL, [])) and self.e.EV_ABS not in caps
                keyboard = bool(keys & self.names.keys())
                if source.name.startswith(('mouse-modes:', 'mouse-modes-test')) or not (mouse or keyboard):
                    source.close()
                    continue
                # Combined absolute touchpad/tablet devices are never grabbed.
                if self.e.EV_ABS in caps:
                    source.close()
                    continue
                relay_caps = {kind: codes for kind, codes in caps.items()
                              if kind in (self.e.EV_KEY, self.e.EV_REL, self.e.EV_MSC)}
                if mouse:
                    relay_caps[self.e.EV_REL] = sorted(set(relay_caps[self.e.EV_REL]) | {
                        self.e.REL_WHEEL, self.e.REL_HWHEEL,
                        self.e.REL_WHEEL_HI_RES, self.e.REL_HWHEEL_HI_RES})
                virtual = UInput(relay_caps, name='mouse-modes: ' + source.name,
                                 vendor=source.info.vendor, product=source.info.product,
                                 version=source.info.version, bustype=source.info.bustype,
                                 input_props=source.input_props())
                source.grab()
                held = set(source.active_keys())
                device = {'source': source, 'virtual': virtual, 'mouse': mouse,
                          'keyboard': keyboard, 'held': held, 'forwarded': set(), 'motion': [0, 0]}
                self.devices[path] = device
                self.selector.register(source, selectors.EVENT_READ, path)
                if not (self.capturing or self.drain):
                    for code in held:
                        virtual.write(self.e.EV_KEY, code, 1)
                        device['forwarded'].add(code)
                    virtual.syn()
                self.recompute()
            except Exception:
                source.close()
                if virtual:
                    virtual.close()
                raise
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
            self.capturing = True
            self.capture_keys.clear()
            self.capture_blocked = self.keys_down()
            self.capture_deadline = time.monotonic() + 20
            for device in self.devices.values():
                if device['keyboard']:
                    self.release(device)
            self.recompute()
            self.status()
        elif command == 'cancel':
            self.capturing = False
            self.drain = self.keys_down()
            self.capture_keys.clear()
            self.recompute()
            self.status()
        elif command == 'quit':
            raise EOFError
        else:
            raise ValueError('Unknown command')

    def event(self, device, event):
        e = self.e
        virtual = device['virtual']
        if event.type == e.EV_SYN and event.code == e.SYN_DROPPED:
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
            self.recompute()
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
        elif event.type in (e.EV_REL, e.EV_MSC):
            virtual.write_event(event)

    def run(self):
        self.selector.register(sys.stdin.buffer, selectors.EVENT_READ, None)
        buffer = b''
        next_scan = 0
        try:
            while True:
                now = time.monotonic()
                if now >= next_scan:
                    self.scan()
                    next_scan = now + 2
                if self.capturing and now >= self.capture_deadline:
                    self.command({'command': 'cancel'})
                ready = self.selector.select(.2)
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
                        device = self.devices[selected.data]
                        try:
                            for event in device['source'].read():
                                self.event(device, event)
                        except OSError:
                            self.remove(selected.data)
        except EOFError:
            pass
        finally:
            for path in list(self.devices):
                self.remove(path)
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
