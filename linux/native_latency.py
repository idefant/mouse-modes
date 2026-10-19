#!/usr/bin/python3
"""Root-only isolated 1 kHz latency regression with deterministic slow discovery.

Only mm-latency-* devices are discovered. Grabs on relay outputs prevent test
motion reaching the desktop. Inject 120 ms into UInput construction to reproduce
its documented node/udev wait without depending on machine-specific timing.
"""
import argparse
from collections import deque
from contextlib import ExitStack
import json
from pathlib import Path
import queue
import select
import statistics
import subprocess
import threading
import time
from evdev import InputDevice, UInput, ecodes as e, list_devices


def run(helper, modes, duration, discovery_delay=.12, hotplug_enabled=True):
    reports = []
    with ExitStack() as stack:
        mouse = stack.enter_context(UInput({e.EV_KEY: [e.BTN_LEFT], e.EV_REL: [e.REL_X, e.REL_Y]}, name='mm-latency-mouse'))
        keyboard = stack.enter_context(UInput({e.EV_KEY: [e.KEY_F13, e.KEY_F14, e.KEY_F17]}, name='mm-latency-keyboard'))
        code = Path(helper).read_text().replace('paths = set(list_devices())',
            "paths = {p for p in list_devices() if open('/sys/class/input/' + p.rsplit('/',1)[-1] + '/device/name').read().startswith('mm-latency-')}")
        code = code.replace('except OSError:\n                            self.remove(selected.data)', 'except OSError as error:\n                            emit(\"diagnostic\", {\"errno\":error.errno,\"path\":selected.data})\n                            self.remove(selected.data)')
        code = code.replace('self.scan()\n                    next_scan', 'scan_started=time.perf_counter()\n                    self.scan()\n                    emit(\"diagnostic\", {\"scan_ms\":(time.perf_counter()-scan_started)*1000})\n                    next_scan')
        lines = code.splitlines()
        for index in range(len(lines) - 1, -1, -1):
            if lines[index].lstrip().startswith('virtual = UInput('):
                indent = lines[index][:-len(lines[index].lstrip())]
                lines.insert(index, indent + f'time.sleep({discovery_delay})  # deterministic slow udev discovery')
        process = subprocess.Popen(['/usr/bin/python3', '-I', '-u', '-c', '\n'.join(lines)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        stack.callback(lambda: process.poll() is None and process.kill())
        messages = queue.Queue()
        def read_status():
            for line in process.stdout:
                messages.put(json.loads(line))
        threading.Thread(target=read_status, daemon=True).start()
        while not messages.get(timeout=10)['payload'].get('available'):
            pass
        output = None
        for path in list_devices():
            device = InputDevice(path)
            if device.name == 'mouse-modes: mm-latency-mouse':
                output = device
                stack.callback(output.close)
                output.grab()
                break
            device.close()
        assert output is not None
        bindings = dict.fromkeys(('slow', 'fast', 'scrollHorizontal', 'scrollVertical', 'scrollBoth'))
        bindings.update(slow={'keys':['F13']}, fast={'keys':['F14']}, scrollBoth={'keys':['F17']})
        process.stdin.write((json.dumps({'command':'settings','settings':{'hotkeys':bindings,'autostart':False}})+'\n').encode())
        process.stdin.flush()
        time.sleep(.1)
        for mode in modes:
            code = {'slow':e.KEY_F13,'fast':e.KEY_F14,'scroll':e.KEY_F17}.get(mode)
            if code:
                keyboard.write(e.EV_KEY, code, 1)
                keyboard.syn()
                time.sleep(.3)
            while select.select([output.fd], [], [], 0)[0]:
                list(output.read())
            sent = deque()
            values = []
            timestamps = []
            failure = []
            stop = threading.Event()
            count = round(duration * 1000)
            def hotplugger():
                if not hotplug_enabled:
                    return
                time.sleep(.7)
                hotplug = UInput({e.EV_REL:[e.REL_X,e.REL_Y]}, name='mm-latency-hotplug-' + mode)
                time.sleep(1.9)
                hotplug.close()
            hotplug_worker = threading.Thread(target=hotplugger)
            hotplug_worker.start()
            def writer():
                try:
                    start = time.perf_counter()
                    for index in range(count):
                        if stop.is_set():
                            break
                        due = start + index / 1000
                        time.sleep(max(0, due - time.perf_counter()))
                        sent.append(time.time_ns())
                        mouse.write(e.EV_REL, e.REL_X, 20)
                        mouse.write(e.EV_REL, e.REL_Y, 20)
                        mouse.syn()
                except Exception as error:
                    failure.append(error)
            worker = threading.Thread(target=writer)
            worker.start()
            deadline = time.monotonic() + duration + 5
            latency = []
            ended = False
            while len(latency) < count and time.monotonic() < deadline:
                if select.select([output.fd], [], [], .1)[0]:
                    try:
                        packet = list(output.read())
                    except BlockingIOError:
                        continue
                    except OSError as error:
                        failure.append(str(error))
                        ended = True
                        break
                    for event in packet:
                        if event.type == e.EV_SYN and event.code == e.SYN_REPORT:
                            latency.append((event.sec * 1_000_000_000 + event.usec * 1000 - sent.popleft()) / 1e6)
            stop.set()
            worker.join()
            hotplug_worker.join()
            if ended:
                time.sleep(.3)
                statuses = []
                while not messages.empty():
                    statuses.append(messages.get_nowait())
                report = {'mode':mode,'received':len(latency),'expected':count,'helper_exited':process.poll(), 'read_error':failure, 'statuses':statuses}
                print(json.dumps(report), flush=True)
                return [report]
            assert not failure, failure
            assert len(latency) == count, (mode, len(latency), count)
            latency.sort()
            result = {'mode':mode,'packets':count,'p95_ms':round(latency[int(count*.95)],3),
                      'p99_ms':round(latency[int(count*.99)],3),'max_ms':round(max(latency),3)}
            reports.append(result)
            print(json.dumps(result), flush=True)
            if code:
                keyboard.write(e.EV_KEY, code, 0)
                keyboard.syn()
                time.sleep(.3)
        process.stdin.close()
        assert process.wait(timeout=5) == 0, process.stderr.read().decode()
    return reports


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('helper', nargs='?', default=str(Path(__file__).with_name('mouse_modes_helper.py')))
    parser.add_argument('--baseline', action='store_true')
    parser.add_argument('--duration', type=float, default=4.5)
    parser.add_argument('--discovery-delay', type=float, default=.12)
    parser.add_argument('--mode', choices=['idle','slow','fast','scroll'])
    parser.add_argument('--no-hotplug', action='store_true')
    args = parser.parse_args()
    results = run(args.helper, [args.mode] if args.mode else ['idle'] if args.baseline else ['idle','slow','fast','scroll'], args.duration, args.discovery_delay, not args.no_hotplug)
    if not args.baseline:
        # A generous CI threshold still catches a 120 ms blocking discovery wait.
        assert all(result.get('p99_ms', float('inf')) < 40 and result.get('max_ms', float('inf')) < 100 for result in results), results
