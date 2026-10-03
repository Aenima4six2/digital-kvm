#!/bin/sh
set -eu
script_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(dirname -- "$script_root")
mode=auto
dry_run=false
config_source=
for arg in "$@"; do
    case "$arg" in --login) mode=login;; --boot) mode=boot;; --dry-run) dry_run=true;; --*) echo "Unknown option: $arg" >&2; exit 1;; *) config_source=$arg;; esac
done
[ "$(uname -m)" = arm64 ] || { echo 'Native Mac monitor control currently requires Apple Silicon.' >&2; exit 1; }
if [ -x "$script_root/digital-kvm" ]; then
    source_exe="$script_root/digital-kvm"
    default_config="$script_root/config.json"
else
    source_exe="${CARGO_TARGET_DIR:-$project_root/target}/release/digital-kvm"
    default_config="$project_root/examples/macos.json"
    [ -x "$source_exe" ] || { echo 'Run cargo build --release --locked as your user, or extract a Mac release package.' >&2; exit 1; }
fi
if [ "$mode" != login ]; then
    [ "$(id -u)" = 0 ] || { echo 'Boot setup requires sudo. Run sudo sh install-macos.sh, or choose --login.' >&2; exit 1; }
    install_root='/Library/Application Support/DigitalKvm'
    plist='/Library/LaunchDaemons/local.digital-kvm.plist'
    domain=system
else
    [ "$(id -u)" != 0 ] || { echo 'Run --login as your desktop user.' >&2; exit 1; }
    [ ! -f /Library/LaunchDaemons/local.digital-kvm.plist ] || { echo 'Remove the existing boot service with sudo sh uninstall-macos.sh before selecting login startup.' >&2; exit 1; }
    install_root="$HOME/Library/Application Support/DigitalKvm"
    plist="$HOME/Library/LaunchAgents/local.digital-kvm.plist"
    domain="gui/$(id -u)"
fi
label=local.digital-kvm
if [ -f "$plist" ]; then
    old_exe=$(plutil -extract ProgramArguments.0 raw -o - "$plist")
    [ "$old_exe" = "$install_root/bin/digital-kvm" ] || { echo 'An unrelated launchd plist occupies the destination.' >&2; exit 1; }
    launchctl bootout "$domain/$label" >/dev/null 2>&1 || true
    rm -- "$plist"
fi
# Also stop manual runs of this installed executable, then wait before copying.
stop_binary() {
    target_exe=$1
    for process_id in $(pgrep -x digital-kvm || true); do
        running_exe=$(ps -ww -p "$process_id" -o comm= | sed 's/^ *//')
        [ "$running_exe" = "$target_exe" ] || continue
        kill -TERM "$process_id" 2>/dev/null || true
        tries=0
        while kill -0 "$process_id" 2>/dev/null && [ "$tries" -lt 50 ]; do sleep 0.1; tries=$((tries+1)); done
        if kill -0 "$process_id" 2>/dev/null; then kill -KILL "$process_id"; fi
    done
}
stop_binary "$install_root/bin/digital-kvm"
if [ "$domain" = system ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    desktop_home=$(dscl . -read "/Users/$SUDO_USER" NFSHomeDirectory | cut -d ' ' -f 2-)
    desktop_uid=$(id -u "$SUDO_USER")
    desktop_plist="$desktop_home/Library/LaunchAgents/local.digital-kvm.plist"
    if [ -f "$desktop_plist" ] && [ "$(plutil -extract ProgramArguments.0 raw -o - "$desktop_plist")" = "$desktop_home/Library/Application Support/DigitalKvm/bin/digital-kvm" ]; then
        launchctl bootout "gui/$desktop_uid/$label" >/dev/null 2>&1 || true
        rm -- "$desktop_plist"
    fi
    stop_binary "$desktop_home/Library/Application Support/DigitalKvm/bin/digital-kvm"
fi
mkdir -p "$install_root/bin" "$(dirname -- "$plist")"
config_path="$install_root/config.json"
if [ -z "$config_source" ]; then
    if [ -f "$config_path" ]; then config_source=$config_path; else config_source=$default_config; fi
fi
if grep -q 'REPLACE_WITH_' "$config_source"; then
    "$source_exe" learn-switch --config "$config_source" --output "$config_path"
    config_source=$config_path
fi
"$source_exe" validate --config "$config_source"
install -m 755 "$source_exe" "$install_root/bin/digital-kvm"
if [ "$config_source" != "$config_path" ]; then install -m 644 "$config_source" "$config_path"; fi
make_plist() {
    output=$1
    command=$2
    job_label=$3
    plutil -create xml1 "$output"
    plutil -insert Label -string "$job_label" "$output"
    plutil -insert ProgramArguments -json '[]' "$output"
    plutil -insert ProgramArguments.0 -string "$install_root/bin/digital-kvm" "$output"
    plutil -insert ProgramArguments.1 -string "$command" "$output"
    plutil -insert ProgramArguments.2 -string --config "$output"
    plutil -insert ProgramArguments.3 -string "$config_path" "$output"
    plutil -insert RunAtLoad -bool true "$output"
    plutil -insert StandardOutPath -string "$install_root/launchd-out.log" "$output"
    plutil -insert StandardErrorPath -string "$install_root/launchd-error.log" "$output"
    chmod 644 "$output"
    if [ "$domain" = system ]; then chown root:wheel "$output"; fi
}
if [ "$domain" = system ]; then
    probe_plist='/Library/LaunchDaemons/local.digital-kvm.probe.plist'
    [ ! -e "$probe_plist" ] || { echo 'An existing probe plist must be removed first.' >&2; exit 1; }
    probe_output="$install_root/boot-probe.json"
    cleanup_probe() { launchctl bootout system/local.digital-kvm.probe >/dev/null 2>&1 || true; rm -f -- "$probe_plist"; }
    trap cleanup_probe EXIT HUP INT TERM
    make_plist "$probe_plist" probe local.digital-kvm.probe
    plutil -insert ProgramArguments.4 -string --force "$probe_plist"
    plutil -replace StandardOutPath -string "$probe_output" "$probe_plist"
    : > "$probe_output"
    launchctl bootstrap system "$probe_plist"
    tries=0
    reachable=false
    while [ "$tries" -lt 100 ]; do
        reachable=$(plutil -extract reachable raw -o - "$probe_output" 2>/dev/null || printf false)
        if [ "$reachable" = true ]; then break; fi
        sleep 0.1
        tries=$((tries+1))
    done
    cleanup_probe
    trap - EXIT HUP INT TERM
    if [ "$reachable" != true ]; then
        if [ "$mode" = auto ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
            echo "Boot monitor access was not demonstrated. Installing login fallback; evidence: $probe_output"
            if [ "$dry_run" = true ]; then
                exec sudo -u "$SUDO_USER" sh "$0" --login --dry-run "$config_path"
            else
                exec sudo -u "$SUDO_USER" sh "$0" --login "$config_path"
            fi
        fi
        echo "Boot probe failed. Inspect $probe_output and launchd-error.log, then run this installer with --login as your user." >&2
        exit 1
    fi
fi
make_plist "$plist" run "$label"
plutil -insert ProgramArguments.4 -string --log "$plist"
plutil -insert ProgramArguments.5 -string "$install_root/digital-kvm.log" "$plist"
plutil -insert ProgramArguments.6 -string --background "$plist"
if [ "$dry_run" = true ]; then plutil -insert ProgramArguments.7 -string --dry-run "$plist"; fi
plutil -insert KeepAlive -bool true "$plist"
plutil -insert ThrottleInterval -integer 10 "$plist"
plutil -lint "$plist"
launchctl bootstrap "$domain" "$plist"
sleep 0.3
launchctl print "$domain/$label" | grep -q 'state = running' || { echo "Service did not stay running; inspect $install_root/launchd-error.log" >&2; exit 1; }
echo "Installed $domain launchd service; configuration: $config_path"
