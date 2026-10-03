#!/bin/sh
set -eu
if [ "$(id -u)" = 0 ]; then
    plist='/Library/LaunchDaemons/local.digital-kvm.plist'
    executable='/Library/Application Support/DigitalKvm/bin/digital-kvm'
    domain=system
else
    plist="$HOME/Library/LaunchAgents/local.digital-kvm.plist"
    executable="$HOME/Library/Application Support/DigitalKvm/bin/digital-kvm"
    domain="gui/$(id -u)"
fi
if [ -f "$plist" ] && [ "$(plutil -extract ProgramArguments.0 raw -o - "$plist")" = "$executable" ]; then
    launchctl bootout "$domain/local.digital-kvm" >/dev/null 2>&1 || true
    rm -- "$plist"
fi
echo 'launchd startup removed. Executable, configuration, and logs are retained.'
