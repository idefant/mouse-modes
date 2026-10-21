#!/bin/sh
set -eu
# Debian calls prerm with upgrade; RPM passes 1 while another version remains.
case "${1:-}" in upgrade|1) exit 0 ;; esac
systemctl disable mouse-modes-input.socket || true
if [ -d /run/systemd/system ]; then
    systemctl stop mouse-modes-input.socket || true
    systemctl stop 'mouse-modes-input@*.service' || true
fi
