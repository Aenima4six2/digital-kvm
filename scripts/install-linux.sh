#!/bin/sh
set -eu
script_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(dirname -- "$script_root")
mode=boot
dry_run=false
config_source=
for arg in "$@"; do
    case "$arg" in --login) mode=login;; --boot) mode=boot;; --dry-run) dry_run=true;; --*) echo "Unknown option: $arg" >&2; exit 1;; *) config_source=$arg;; esac
done
if [ -x "$script_root/digital-kvm" ]; then
    source_exe="$script_root/digital-kvm"
    default_config="$script_root/config.json"
else
    cd "$project_root"
    source_exe="${CARGO_TARGET_DIR:-$project_root/target}/release/digital-kvm"
    if [ ! -x "$source_exe" ]; then
        command -v cargo >/dev/null 2>&1 || { echo 'Build as your user first, or extract a Linux release package.' >&2; exit 1; }
        cargo test --locked
        cargo build --release --locked
    fi
    default_config="$project_root/examples/linux.json"
fi
command -v systemctl >/dev/null 2>&1 || { echo 'This installer requires systemd.' >&2; exit 1; }
if [ "$mode" = boot ]; then
    [ "$(id -u)" = 0 ] || { echo 'Boot installation requires sudo. Run sudo sh install-linux.sh, or choose --login.' >&2; exit 1; }
    install_root=/usr/local/lib/digital-kvm
    config_root=/etc/digital-kvm
    unit_root=/etc/systemd/system
else
    [ "$(id -u)" != 0 ] || { echo 'Run --login as the intended desktop user.' >&2; exit 1; }
    install_root="${XDG_DATA_HOME:-$HOME/.local/share}/digital-kvm"
    config_root="${XDG_CONFIG_HOME:-$HOME/.config}/digital-kvm"
    unit_root="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
fi
config_path="$config_root/config.json"
unit_path="$unit_root/digital-kvm.service"
if [ -f "$unit_path" ] && ! grep -Fq "$install_root/bin/digital-kvm" "$unit_path"; then
    echo 'An unrelated systemd unit occupies the destination.' >&2
    exit 1
fi
if [ "$mode" = login ] && [ -f /etc/systemd/system/digital-kvm.service ]; then
    echo 'Remove the existing boot service with sudo sh uninstall-linux.sh before selecting login startup.' >&2
    exit 1
fi
if [ "$mode" = boot ]; then systemctl stop digital-kvm.service >/dev/null 2>&1 || true; else systemctl --user stop digital-kvm.service >/dev/null 2>&1 || true; fi
# Stop manual runs of the installed executable, without matching argv text.
stop_binary() {
    target_exe=$1
    for link in /proc/[0-9]*/exe; do
        running_exe=$(readlink "$link" 2>/dev/null || true)
        case "$running_exe" in "$target_exe"|"$target_exe (deleted)") ;; *) continue;; esac
        process_id=${link#/proc/}
        process_id=${process_id%/exe}
        kill -TERM "$process_id" 2>/dev/null || true
        tries=0
        while kill -0 "$process_id" 2>/dev/null && [ "$tries" -lt 50 ]; do sleep 0.1; tries=$((tries+1)); done
        if kill -0 "$process_id" 2>/dev/null; then kill -KILL "$process_id"; fi
    done
}
stop_binary "$install_root/bin/digital-kvm"
if [ "$mode" = boot ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    desktop_home=$(getent passwd "$SUDO_USER" | cut -d : -f 6)
    desktop_uid=$(id -u "$SUDO_USER")
    desktop_unit="$desktop_home/.config/systemd/user/digital-kvm.service"
    if [ -f "$desktop_unit" ] && grep -Fq "$desktop_home/.local/share/digital-kvm/bin/digital-kvm" "$desktop_unit"; then
        runuser -u "$SUDO_USER" -- env XDG_RUNTIME_DIR="/run/user/$desktop_uid" systemctl --user disable --now digital-kvm.service
        rm -- "$desktop_unit"
        runuser -u "$SUDO_USER" -- env XDG_RUNTIME_DIR="/run/user/$desktop_uid" systemctl --user daemon-reload
    fi
    stop_binary "$desktop_home/.local/share/digital-kvm/bin/digital-kvm"
fi
if [ -z "$config_source" ]; then
    if [ -f "$config_path" ]; then config_source=$config_path; else config_source=$default_config; fi
fi
mkdir -p "$config_root"
if grep -q 'REPLACE_WITH_' "$config_source"; then
    "$source_exe" learn-switch --config "$config_source" --output "$config_path"
    config_source=$config_path
fi
"$source_exe" validate --config "$config_source"
if [ "$mode" = boot ]; then
    systemctl stop digital-kvm.service >/dev/null 2>&1 || true
    # Give only GPU DDC adapters to a dedicated service group, without a login ACL.
    getent group digital-kvm-ddc >/dev/null || groupadd --system digital-kvm-ddc
    # A persistent account also works with D-Bus daemons that reject dynamic UIDs.
    getent -s files passwd digital-kvm >/dev/null || useradd --system --gid digital-kvm-ddc --home-dir /var/lib/digital-kvm --no-create-home --shell /usr/sbin/nologin digital-kvm
    mkdir -p /etc/udev/rules.d /etc/modules-load.d
    printf '%s\n' 'i2c-dev' > /etc/modules-load.d/digital-kvm.conf
    printf '%s\n' '# Digital KVM: GPU DDC adapters' 'SUBSYSTEM=="i2c-dev", SUBSYSTEMS=="pci", ATTRS{class}=="0x03*", GROUP="digital-kvm-ddc", MODE="0660"' 'SUBSYSTEM=="i2c-dev", SUBSYSTEMS=="drm", GROUP="digital-kvm-ddc", MODE="0660"' > /etc/udev/rules.d/60-digital-kvm-ddc.rules
    if [ "$dry_run" = false ]; then
        modprobe i2c-dev
        udevadm control --reload-rules
        udevadm trigger --subsystem-match=i2c-dev
        udevadm settle
    fi
else
    systemctl --user stop digital-kvm.service >/dev/null 2>&1 || true
fi
mkdir -p "$install_root/bin" "$config_root" "$unit_root"
install -m 755 "$source_exe" "$install_root/bin/digital-kvm"
if [ "$config_source" != "$config_path" ]; then install -m 644 "$config_source" "$config_path"; fi
if [ "$mode" = boot ]; then
    cat > "$unit_root/digital-kvm.service" <<'EOF'
[Unit]
Description=Digital KVM USB switch monitor bridge
After=systemd-udev-trigger.service systemd-logind.service
[Service]
Type=simple
User=digital-kvm
Group=digital-kvm-ddc
SupplementaryGroups=digital-kvm-ddc
StateDirectory=digital-kvm
Environment=XDG_DATA_HOME=/var/lib
Restart=always
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
EOF
else
    printf '%s\n' '[Unit]' 'Description=Digital KVM USB switch monitor bridge' '[Service]' 'Type=simple' 'Restart=always' 'RestartSec=5' > "$unit_root/digital-kvm.service"
fi
# systemd quoted arguments still expand percent specifiers; escape those too.
escape_unit() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g'; }
exe_escaped=$(escape_unit "$install_root/bin/digital-kvm")
config_escaped=$(escape_unit "$config_path")
extra=
if [ "$dry_run" = true ]; then extra=' --dry-run'; fi
printf 'ExecStart="%s" run --background --config "%s"%s\n' "$exe_escaped" "$config_escaped" "$extra" >> "$unit_root/digital-kvm.service"
printf '\n[Install]\nWantedBy=%s\n' "$(if [ "$mode" = boot ]; then printf multi-user.target; else printf default.target; fi)" >> "$unit_root/digital-kvm.service"
if [ "$mode" = boot ]; then
    systemctl daemon-reload
    systemctl enable --now digital-kvm.service
    sleep 0.3
    systemctl is-active --quiet digital-kvm.service || { journalctl -u digital-kvm.service --no-pager -n 10; exit 1; }
    systemctl --no-pager status digital-kvm.service
else
    systemctl --user daemon-reload
    systemctl --user enable --now digital-kvm.service
    sleep 0.3
    systemctl --user is-active --quiet digital-kvm.service || { journalctl --user -u digital-kvm.service --no-pager -n 10; exit 1; }
    systemctl --user --no-pager status digital-kvm.service
fi
echo "Installed $mode service; configuration: $config_path"
