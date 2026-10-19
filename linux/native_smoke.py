#!/usr/bin/python3
"""Opt-in root integration test with isolated synthetic input devices only.

No real device is grabbed. Virtual mouse output is grabbed before motion is
emitted. Outside recording, standalone synthetic F13-F17 and Ctrl key edges
are visible to the desktop; run this fixture without concurrent keyboard use.
"""
import json
from pathlib import Path
import queue
import select
import subprocess
import threading
import time
from contextlib import ExitStack
from evdev import InputDevice, UInput, ecodes as e, list_devices


def check(value, description):
    if not value:
        raise AssertionError(description)
    print('PASS ' + description, flush=True)


def main():
    with ExitStack() as stack:
        mouse = stack.enter_context(UInput({e.EV_KEY: [e.BTN_LEFT], e.EV_REL: [e.REL_X, e.REL_Y]},
                                           name='mm-native-test-mouse'))
        keyboard = stack.enter_context(UInput({e.EV_KEY: [e.KEY_F13, e.KEY_F14, e.KEY_F15,
            e.KEY_F16, e.KEY_F17, e.KEY_LEFTCTRL, e.KEY_RIGHTCTRL, e.KEY_A]}, name='mm-native-test-keyboard'))
        code = Path(__file__).with_name('mouse_modes_helper.py').read_text()
        # Change discovery only. All event processing/IPC/grab code is production code.
        code = code.replace('paths = set(list_devices())',
            "paths = {p for p in list_devices() if InputDevice(p).name.startswith('mm-native-test-')}")
        process = subprocess.Popen(['/usr/bin/python3', '-I', '-u', '-c', code],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        stack.callback(lambda: process.poll() is None and process.kill())
        events = queue.Queue()
        def reader():
            for line in process.stdout:
                events.put(json.loads(line))
        threading.Thread(target=reader, daemon=True).start()
        def event(name, predicate=lambda _: True):
            end = time.monotonic() + 5
            while time.monotonic() < end:
                message = events.get(timeout=5)
                if message['event'] == name and predicate(message['payload']):
                    return message['payload']
            raise AssertionError('Expected event: ' + name)
        def command(name, **values):
            process.stdin.write((json.dumps({'command': name, **values}) + '\n').encode())
            process.stdin.flush()
        event('engine-status', lambda status: status['available'])
        outputs = {}
        for path in list_devices():
            device = InputDevice(path)
            if device.name.startswith('mouse-modes: mm-native-test-'):
                stack.callback(device.close)
                device.grab()
                outputs['mouse' if device.name.endswith('mouse') else 'keyboard'] = device
            else:
                device.close()
        check(len(outputs) == 2, 'real evdev discovery and two prepared uinput outputs')
        def drain(device, wait=.04):
            result = []
            while select.select([device.fd], [], [], wait)[0]:
                result.extend(device.read())
                wait = .005
            return result
        def key(code, value):
            keyboard.write(e.EV_KEY, code, value)
            keyboard.syn()
            time.sleep(.025)
        def motion(dx, dy):
            drain(outputs['mouse'])
            mouse.write(e.EV_REL, e.REL_X, dx)
            mouse.write(e.EV_REL, e.REL_Y, dy)
            mouse.syn()
            values = {}
            for item in drain(outputs['mouse']):
                if item.type == e.EV_REL:
                    values[item.code] = values.get(item.code, 0) + item.value
            return tuple(values.get(code, 0) for code in (e.REL_X, e.REL_Y, e.REL_HWHEEL, e.REL_WHEEL))
        bindings = dict.fromkeys(('slow', 'fast', 'scrollHorizontal', 'scrollVertical', 'scrollBoth'))
        def settings():
            command('settings', settings={'hotkeys': bindings, 'autostart': False})
            time.sleep(.06)
        settings()
        key(e.KEY_F13, 1)
        mouse.device.grab()
        mouse.device.ungrab()
        check(True, 'empty defaults leave physical mouse ungrabbed')
        bindings.update({name: {'keys': ['F' + str(index + 13)]} for index, name in enumerate(bindings)})
        settings()
        check(motion(20, -20) == (5, -5, 0, 0), 'settings apply to an already-held key without restart')
        key(e.KEY_F14, 1)
        check(motion(20, -20) == (40, -40, 0, 0), 'fast overrides slow')
        key(e.KEY_F14, 0)
        key(e.KEY_F13, 0)
        for code, expected in ((e.KEY_F15, (0, 0, 2, 0)), (e.KEY_F16, (0, 0, 0, -3)),
                               (e.KEY_F17, (0, 0, 2, -3))):
            key(code, 1)
            check(motion(40, 60) == expected, 'held scroll transform ' + str(code))
            key(code, 0)
        # The mouse stays prearmed with configured bindings: a drag begun before
        # a precision key must preserve one down/up pair while changing speed.
        drain(outputs['mouse'])
        mouse.write(e.EV_KEY, e.BTN_LEFT, 1)
        mouse.syn()
        time.sleep(.03)
        check([item.value for item in drain(outputs['mouse']) if item.type == e.EV_KEY] == [1], 'drag button down forwarded once')
        key(e.KEY_F13, 1)
        check(motion(20, 0) == (5, 0, 0, 0), 'precision mode activates during an existing virtual drag')
        key(e.KEY_F13, 0)
        mouse.write(e.EV_KEY, e.BTN_LEFT, 0)
        mouse.syn()
        releases = [item.value for item in drain(outputs['mouse'])
                    if item.type == e.EV_KEY and item.code == e.BTN_LEFT]
        check(releases == [0], 'drag releases once after mode transitions')
        bindings['slow'] = {'keys': ['ControlLeft']}
        settings()
        key(e.KEY_RIGHTCTRL, 1)
        check(motion(20, 0) == (20, 0, 0, 0), 'right modifier does not trigger left binding')
        key(e.KEY_RIGHTCTRL, 0)
        key(e.KEY_LEFTCTRL, 1)
        check(motion(20, 0) == (5, 0, 0, 0), 'left modifier triggers left binding')
        key(e.KEY_LEFTCTRL, 0)
        bindings['slow'] = {'keys': ['Control']}
        settings()
        for code in (e.KEY_LEFTCTRL, e.KEY_RIGHTCTRL):
            key(code, 1)
            check(motion(20, 0) == (5, 0, 0, 0), 'any-side modifier ' + str(code))
            key(code, 0)
        drain(outputs['keyboard'])
        command('capture')
        event('engine-status', lambda status: status['capturing'])
        for code, value in ((e.KEY_LEFTCTRL, 1), (e.KEY_RIGHTCTRL, 1), (e.KEY_A, 1),
                            (e.KEY_A, 2), (e.KEY_LEFTCTRL, 0), (e.KEY_A, 0), (e.KEY_RIGHTCTRL, 0)):
            key(code, value)
        captured = event('hotkey-captured')
        check(captured == {'keys': ['Control', 'KeyA']}, 'native chord records both modifier sides as any-side')
        check(not [item for item in drain(outputs['keyboard']) if item.type == e.EV_KEY],
              'capture forwards zero keyboard down, repeat, or up events')
        event('engine-status', lambda status: not status['capturing'])
        command('capture')
        event('engine-status', lambda status: status['capturing'])
        key(e.KEY_A, 1)
        command('cancel')
        event('engine-status', lambda status: not status['capturing'])
        key(e.KEY_A, 2)
        key(e.KEY_A, 0)
        check(not [item for item in drain(outputs['keyboard']) if item.type == e.EV_KEY],
              'cancel drains held key without replaying repeats or release')
        # A second relative mouse can appear and disappear without restarting.
        hotplug = UInput({e.EV_KEY: [e.BTN_LEFT], e.EV_REL: [e.REL_X, e.REL_Y]},
                        name='mm-native-test-hotplug')
        stack.callback(hotplug.close)
        event('engine-status', lambda status: 'мышей — 2' in status['message'])
        hotplug_output = None
        for path in list_devices():
            candidate = InputDevice(path)
            if candidate.name == 'mouse-modes: mm-native-test-hotplug':
                hotplug_output = candidate
                break
            candidate.close()
        check(hotplug_output is not None, 'hotplug discovers an additional relative mouse')
        stack.callback(hotplug_output.close)
        hotplug_output.grab()
        hotplug.write(e.EV_REL, e.REL_X, 7)
        hotplug.syn()
        check(any(item.type == e.EV_REL and item.code == e.REL_X and item.value == 7
                  for item in drain(hotplug_output)), 'hotplugged mouse is relayed')
        hotplug.close()
        event('engine-status', lambda status: 'мышей — 1' in status['message'])
        check(process.poll() is None, 'unplugging a mouse keeps the remaining relay running')
        process.stdin.close()
        check(process.wait(timeout=5) == 0, 'stdin EOF exits root helper cleanly')
        for source in (mouse.device, keyboard.device):
            source.grab()
            source.ungrab()
        check(True, 'helper exit releases both source grabs')
        check(not process.stderr.read(), 'native helper produced no stderr errors')


if __name__ == '__main__':
    main()
