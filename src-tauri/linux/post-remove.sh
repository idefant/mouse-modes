#!/bin/sh
set -eu
case "${1:-}" in upgrade|1) exit 0 ;; esac
if [ -c /dev/uinput ]; then
    setfacl -x u:mouse-modes /dev/uinput 2>/dev/null || true
fi
udevadm control --reload-rules 2>/dev/null || true
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi
