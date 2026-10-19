# Linux input helper

Runtime dependencies: Linux uinput, polkit (`pkexec`), `/usr/bin/python3`,
Python `evdev`, and a graphical polkit authentication agent. Arch/Omarchy names
the Python package `python-evdev`. The GUI remains unprivileged; a separate
helper uses explicit polkit authorization. No permissive polkit rule or input
group membership is installed. The embedded helper runs Python isolated mode
and communicates only over inherited JSON stdio pipes. Closing the pipe releases
all grabs. It emits status/completed intentional chords, never typed-key streams.

## Input ownership and latency

All input-device enumeration, capability probing, virtual-device creation and
virtual-device destruction run in a discovery worker. UInput's node/udev lookup
can wait up to 1.9 seconds; none of that work belongs in the input event loop.
The worker caches known node identities, including excluded devices. Only the
event loop changes grabs, selectors, held keys or mode state. Readiness yielding
EAGAIN is retried without removing/recreating the device.

With five empty bindings, mice remain ungrabbed and their movement is handled
by the normal desktop path. Virtual outputs are prepared in the background.
With configured bindings, mice stay relayed even while no mode key is held.
This preserves button ownership during dragging and lets precision/scroll modes
start immediately while a button is already down. Hotkey presses never switch
physical/virtual mouse ownership. Changing settings between empty/non-empty
bindings waits for existing mouse buttons to be released before switching
ownership, preventing lost releases. Absolute touchpads/tablets are excluded. Device selection checks actual pointer
axes/touch buttons rather than rejecting all EV_ABS capabilities: ZMK Cornechon
keyboards expose ABS_VOLUME alongside ordinary keys and are accepted. Non-pointer
absolute consumer axes remain present on virtual devices and are forwarded when
a mixed device is relayed.

Keyboards are observed without exclusive grabs outside recording. Capture waits
for existing keys to be released, then exclusively grabs keyboards before its
acknowledgement is sent to the UI. On mixed keyboard/mouse devices, existing
mouse buttons must also be released before capture starts. Down/repeat/up events
are suppressed throughout recording. Completion, cancel and the 20-second
timeout release the grab only after recorded keys are physically released.
Both modifier sides normalize to any-side; a single side remains distinct.

Fast wins over slow; Linux factors are .25×/2× with fractional movement retained.
Scrolling freezes both pointer axes and converts 20 raw units into one notch,
capped at 20 notches per axis per packet. Settings update while keys are held.
Buttons/native/high-resolution wheels are preserved. Unexpected active-relay
input overflow stops safely and releases devices; restart to reconnect.

## Verification

Pure tests (no root/evdev):

```sh
python3 -m unittest discover -s linux -v
```

Opt-in native tests require root and use synthetic devices only. They never grab
real user input. Mouse outputs are grabbed to prevent cursor movement. Standalone
synthetic F13-F17/Ctrl edges outside recording remain visible to the desktop:
run unattended, without concurrent keyboard use.

```sh
python3 linux/native_smoke.py
# Optional: copy only a real keyboard's capabilities into the synthetic fixture.
# This never reads or relays the real device's key events.
python3 linux/native_smoke.py --clone-keyboard /dev/input/event5
python3 linux/native_latency.py
python3 linux/native_latency.py --mode fast --discovery-delay 1.9
```

`native_latency.py` sends 4,500 packets per mode at 1 kHz with configured
bindings, adds/removes another device while streaming, and injects a deterministic
120 ms discovery wait by default. It measures kernel output timestamp minus
injection timestamp, avoiding observer scheduling artifacts. Counts, p95, p99
and max are printed; missing packets/helper exit or excessive lag fail the run.
`--discovery-delay 0 --no-hotplug` gives the ordinary steady-state control.
An old helper may be passed as a positional path with `--baseline` to reproduce
the defect without expecting the run to pass.

The old event loop blocks for ~122 ms during the deterministic discovery wait,
then hits SYN_DROPPED and stops before all 4,500 packets arrive. Its clean steady
control passes; the tests establish a blocking-discovery defect, not a direct
profile of the user's physical-device stutter. The corrected worker survives
slow discovery while existing configured modes keep forwarding. Physical input
and compositor acceptance still need user testing; these tests do not establish
an absolute latency bound for arbitrary CPU load, polling rates or devices.
