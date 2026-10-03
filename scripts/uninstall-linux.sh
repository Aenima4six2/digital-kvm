#!/bin/sh
set -eu
if [ "$(id -u)" = 0 ]; then
    unit=/etc/systemd/system/digital-kvm.service
    if [ -f "$unit" ] && grep -q '/usr/local/lib/digital-kvm/bin/digital-kvm' "$unit"; then
        systemctl disable --now digital-kvm.service
        rm -- "$unit"
        systemctl daemon-reload
    fi
else
    unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/digital-kvm.service"
    if [ -f "$unit" ] && grep -q '/digital-kvm/bin/digital-kvm' "$unit"; then
        systemctl --user disable --now digital-kvm.service
        rm -- "$unit"
        systemctl --user daemon-reload
    fi
fi
echo 'Service removed. Executables, configuration, logs, and DDC permission setup are retained.'
