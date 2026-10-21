#!/usr/bin/env python3
"""Install/reinstall/remove a real package in a disposable Ubuntu/Fedora container."""
import glob
import json
import os
from pathlib import Path
import pwd
import socket
import subprocess
import sys
import tempfile

assert Path('/.dockerenv').exists() and os.geteuid() == 0, 'Run only in a disposable root Docker container'
kind, artifacts = sys.argv[1:]
packages = glob.glob(str(Path(artifacts) / '**' / {'deb': '*.deb', 'rpm': '*.rpm', 'arch': '*.pkg.tar.zst'}[kind]), recursive=True)
assert len(packages) == 1, packages
package = packages[0]


def command(*args, allowed=(0,)):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    assert result.returncode in allowed, f'{args}: {result.returncode}\n{result.stdout[-10000:]}'
    return result.stdout


def install(reinstall=False):
    if kind == 'deb':
        command('apt-get', 'install', '-y', package) if not reinstall else command('dpkg', '-i', package)
    elif kind == 'arch':
        command('pacman', '-U', '--noconfirm', package)
    elif reinstall:
        command('rpm', '-U', '--replacepkgs', package)
    else:
        command('dnf', '-y', 'install', package)


install()
account = pwd.getpwnam('mouse-modes')
assert account.pw_uid != 0 and account.pw_shell.endswith(('nologin', 'false'))
try:
    pwd.getpwnam('desktop-test')
except KeyError:
    command('useradd', '-m', 'desktop-test')
assert 'input' not in command('id', '-nG', 'desktop-test').split()
fixture = Path('/home/desktop-test/.config/dev.mouse-modes.app/settings.json')
fixture.parent.mkdir(parents=True, exist_ok=True)
fixture.write_text('{"hotkeys":{"slow":{"keys":["F13"]}},"autostart":true}\n')
settings = fixture.read_bytes()
assert command('systemctl', 'is-enabled', 'mouse-modes-input.socket').strip() == 'enabled'
assert 'not found' not in command('ldd', '/usr/bin/mouse-modes')
assert os.stat('/usr/bin/mouse-modes').st_uid == 0
assert not os.stat('/usr/bin/mouse-modes').st_mode & 0o6022
# No display server: this entry must exit before GUI initialization, as non-root.
root_attempt = command('/usr/bin/mouse-modes', '--input-service', allowed=(1,))
assert 'dedicated unprivileged account' in root_attempt
with tempfile.TemporaryDirectory(prefix='mouse-modes-peer-') as temporary:
    os.chmod(temporary, 0o755)
    path = str(Path(temporary) / 'service.sock')
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(path)
    os.chmod(path, 0o666)
    listener.listen(1)
    code = 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print(s.makefile().readline().strip())'
    client = subprocess.Popen(['runuser', '-u', 'desktop-test', '--', 'python3', '-c', code, path], stdout=subprocess.PIPE, text=True)
    connection, _ = listener.accept()
    worker = subprocess.Popen(['runuser', '-u', 'mouse-modes', '--', '/usr/bin/mouse-modes', '--input-service'], stdin=connection, stdout=connection, stderr=subprocess.PIPE, text=True)
    connection.close()
    output, _ = client.communicate(timeout=10)
    _, error = worker.communicate(timeout=10)
    event = json.loads(output)
    assert not event['payload']['available'] and 'session' in event['payload']['message'].lower(), (event, error)
    assert worker.returncode == 1
    listener.close()
install(reinstall=True)
assert fixture.read_bytes() == settings
assert command('systemctl', 'is-enabled', 'mouse-modes-input.socket').strip() == 'enabled'
if kind == 'deb':
    command('apt-get', 'remove', '-y', 'mouse-modes')
elif kind == 'arch':
    command('pacman', '-R', '--noconfirm', 'mouse-modes')
else:
    command('dnf', '-y', 'remove', 'mouse-modes')
for path in ['/usr/bin/mouse-modes', '/usr/lib/systemd/system/mouse-modes-input.socket', '/usr/lib/systemd/system/mouse-modes-input@.service', '/usr/lib/udev/rules.d/70-mouse-modes.rules']:
    assert not Path(path).exists(), path
assert fixture.read_bytes() == settings
assert not Path('/etc/systemd/system/sockets.target.wants/mouse-modes-input.socket').is_symlink()
print(f'{kind}: clean install, non-root service, no per-user setup, inactive client rejection, reinstall, removal and settings preservation PASS')
