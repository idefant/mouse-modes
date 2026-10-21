#!/bin/sh
set -eu
systemd-sysusers /usr/lib/sysusers.d/mouse-modes.conf
# Chroot/image installation must not require a running host service manager.
modprobe uinput 2>/dev/null || true
udevadm control --reload-rules 2>/dev/null || true
if [ -c /dev/uinput ]; then
    setfacl -m u:mouse-modes:rw /dev/uinput
fi
systemctl enable mouse-modes-input.socket
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl start mouse-modes-input.socket
fi
