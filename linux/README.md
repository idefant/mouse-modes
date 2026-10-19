# Linux input helper

The app needs `polkit` (`pkexec`), `/usr/bin/python3`, Python's `evdev` package,
Linux `uinput`, and a running graphical polkit authentication agent. On Arch /
Omarchy, the Python package is `python-evdev`. These are runtime dependencies;
Python is not embedded in the executable.

The GUI runs as the user. On launch it requests explicit polkit authorization
for a separate Python helper. There is no installed permissive polkit rule,
setuid executable, or input-group membership requirement. The helper is embedded
in the Rust executable and passed directly to Python's isolated mode, rather
than imported from a writable project directory.

The only IPC is inherited standard-input/output pipes. The helper accepts JSON
settings, begin/cancel recording, and quit; it has no command execution or
filesystem-path arguments. It emits engine status and a completed chord, never
a stream of typed keys. Closing the app's pipe releases every grabbed device.
An app crash also closes that pipe. A single `/run` lock prevents two instances
of this helper from grabbing devices simultaneously.

Physical keyboards and relative mice are relayed through separate virtual input
devices. Touchpads/tablets with absolute axes are excluded. Connected devices
are discovered every two seconds. Mouse buttons, native wheel events, and
high-resolution wheels are preserved. Fast mode wins over slow mode. Linux uses
0.25× slow and 2× fast movement, retaining fractional deltas. Scrolling freezes
both pointer axes, converts 20 raw units per wheel notch and caps a packet at
20 notches per axis. Vertical scrolling follows the physical mouse Y direction.

Settings replace the mode bindings while the relay runs, including while keys
are held. By default all five bindings are empty. Matches allow extra held keys,
as in the source AHK wildcard hotkeys.

During capture all keyboard events are suppressed, including repeats and
releases. The backend acknowledges capture before the UI can ask for the chord.
A chord is completed after every keyboard key has been released. Keys already
held when capture starts are released virtually and excluded until physically
released. Cancel/timeout drains remaining held keys without replay. Capture
times out after 20 seconds. Both sides of one modifier normalize to an any-side
modifier. Left and right remain distinct if only one is recorded.

If another relay already owns a physical device (including the earlier test
script), this helper reports an error and releases devices it has opened.
Input queue overflow also stops the relay and releases its grabs rather than
leaving an incorrect mode active. Restart after authorization rejection, device
ownership errors, or an overflow.

Pure tests, requiring neither root nor evdev:

```sh
python3 -m unittest discover -s linux -v
```

Physical input behavior still needs an interactive test on the target machine;
unit tests do not prove compositor acceptance of uinput devices or polkit UI.
