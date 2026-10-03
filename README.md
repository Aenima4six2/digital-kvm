# Digital KVM

A small Windows, macOS, and Linux service that uses a physical USB switch to select monitor inputs. When the configured switch hub connects, it selects this computer's input. When that hub disconnects, it selects the other computer's input.

The Rust controller blocks on native device notifications while idle. It has no GUI, runtime Python dependency, or recurring USB scan. It supports multiple monitors with explicit identities and input mappings.

The local Windows service measured 2.3 MiB private memory and 0.03 seconds additional CPU over roughly eight minutes; the GNU release executable is about 516 KiB. These are local measurements, not cross-platform guarantees.

## Current hardware and validation

The supplied profiles describe the original setup: LG ULTRAGEAR+ with EDID `GSMC4B9`, DDC model `G930B`, and serial `602NTVSHW119`; Windows uses DisplayPort and the Mac uses USB-C. The retail model is inferred to be LG 52G930B-B, pending confirmation from its label. The HYTE case display is excluded.

The USB trigger is the physical switch's outer Genesys hub, `05E3:0610`. Windows observed that hub disappear and return during button presses. The profiles do not select a mouse or keyboard receiver. Its location differs on each host; macOS and Linux setup learn it from an actual disconnect/connect cycle.

This LG accepts alternate DDC commands: source address `0x50`, VCP `0xF4`, value `0xD0` for DisplayPort and `0xD1` for USB-C. Switching **DisplayPort → USB-C → DisplayPort through the Windows DisplayPort connection was visually confirmed**. Windows service-context writes also passed. Standard `0x60` readback did not reliably reflect the alternate switch, so its profiles deliberately leave `readback` empty. Accepted commands are logged as unverified; USB ownership events always send the command.

| Platform | USB events | Monitor transport | Startup |
| --- | --- | --- | --- |
| Windows x64 | Configuration Manager device/hub callbacks | NVIDIA NVAPI; LG alternate and standard DDC | Native automatic SCM service after a service-context probe; login fallback |
| Apple Silicon macOS | IOKit notifications | IORegistry/IOAVService; LG alternate and standard DDC | LaunchDaemon after a boot-context probe; LaunchAgent fallback |
| Linux x64 / ARM64 | libudev | DRM connector EDID and its `/dev/i2c-*` DDC adapter | systemd boot service; explicit user-service alternative |

Mac monitor control requires Apple Silicon and uses private APIs. Windows monitor control currently requires NVIDIA. Linux requires systemd/logind, libudev, libsystemd, and an accessible GPU DDC adapter. Linux service lifecycle was tested under Ubuntu WSL; WSL did not expose the physical monitor or USB switch. Mac and physical Linux DDC behavior require local hardware checks. Full reboot, sleep, and two-host redundancy tests are outstanding.

## Install a release package

Extract the matching archive from [GitHub Releases](https://github.com/Aenima4six2/digital-kvm/releases). Rust is needed only for building from source. Every installer stops the existing service and processes running the installed executable, waits for exit, replaces the binary, preserves the configuration unless a replacement is supplied, and restarts the utility.

The supplied identities belong to the original setup. For another setup, use `devices` and `monitors` and edit `config.json` first.

### Windows

Run in Administrator PowerShell from the extracted directory:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-windows.ps1
```

Default `-Mode Auto` tests a monitor write from a temporary LocalSystem service in Session 0. If accepted, it installs `DigitalKvm` with automatic boot startup; otherwise it installs current-user login startup. `-Mode Boot` requires the probe to pass; `-Mode Login` explicitly selects login startup. Changing an existing boot installation to login startup requires Administrator access.

The probe selects the input corresponding to current switch presence. It tests driver access from the service context; visual firmware confirmation and an actual reboot remain separate tests.

Boot files: `%ProgramFiles%\DigitalKvm\digital-kvm.exe`, `%ProgramData%\DigitalKvm\config.json`, and `%ProgramData%\DigitalKvm\digital-kvm.log`. Login files are under `%LOCALAPPDATA%\DigitalKvm`.

```powershell
.\install-windows.ps1 -Mode Boot -Config .\config.json
.\stop-windows.ps1
.\uninstall-windows.ps1
```

`-DryRun` disables event-driven commands, but the boot capability probe still writes. `-NoStart` installs without starting; `-NoStartup` stages files without registering startup. Uninstall retains binaries, configuration, and logs.

### macOS

From the Apple Silicon package:

```sh
sudo sh install-macos.sh
```

For a new profile, setup asks you to switch away, wait several seconds, and switch back. It saves the hub path observed on this Mac. Discovery sends no monitor commands. If multiple unrelated hubs move, it prints the observed candidates and requires an explicit selector.

Setup tests monitor access in a temporary LaunchDaemon. A passing probe installs system startup; otherwise invocation through `sudo` falls back to the invoking user's LaunchAgent. Use `--boot` to require boot access or `--login` for login startup. Remove an existing boot installation with `sudo sh uninstall-macos.sh` before explicitly selecting login startup.

```sh
sh install-macos.sh --login ./config.json
sudo sh uninstall-macos.sh
```

Boot files live under `/Library/Application Support/DigitalKvm` and `/Library/LaunchDaemons/local.digital-kvm.plist`. Login files use the corresponding home directories. `--dry-run` disables service switching; the boot probe still writes. Uninstall retains binaries, configuration, and logs.

### Linux

From the Linux package on a systemd distribution:

```sh
sudo sh install-linux.sh
```

Setup learns the host-specific hub path when the template contains a placeholder. Boot startup uses a persistent `digital-kvm` system account and `digital-kvm-ddc` group. It loads `i2c-dev` and installs udev permissions for GPU DDC adapters. The driver must expose a DDC bus for the configured DRM connector; monitor control uses no X11 or Wayland APIs.

Files: `/usr/local/lib/digital-kvm/bin/digital-kvm`, `/etc/digital-kvm/config.json`, `/var/lib/digital-kvm/digital-kvm.log`, and `/etc/systemd/system/digital-kvm.service`.

```sh
sudo systemctl status digital-kvm
sudo journalctl -u digital-kvm
sudo sh uninstall-linux.sh
sh install-linux.sh --login ./config.json
```

The login alternative uses a systemd user service and requires user DDC permissions. Remove existing boot startup first. `--dry-run` tests service operation without DDC writes or refreshing GPU permissions. Uninstall retains binaries, configuration, logs, and DDC permission setup.

## CLI and configuration

```text
digital-kvm devices
digital-kvm monitors
digital-kvm validate --config config.json
digital-kvm learn-switch --config config.json --output learned.json
digital-kvm status --config config.json
digital-kvm watch --config config.json --seconds 30
digital-kvm run --config config.json --dry-run
digital-kvm set dp --config config.json --force
digital-kvm probe --config config.json --force
digital-kvm roundtrip --config config.json --hold-ms 8000
```

Options may precede or follow the command. `--force` requires a command. `watch` records USB events and never switches inputs. Stop the service before a manual monitor or roundtrip test so ownership reconciliation does not interfere. `roundtrip` arms a separate restore process before selecting the remote input.

Configuration is strict JSON: unknown fields and unresolved `REPLACE_WITH_` selectors fail validation. IDs are decimal. The original hub selector is:

```json
{"vendor_id":1507,"product_id":1552,"instance_contains":"USB\\VID_05E3&PID_0610\\6&23491980&0&1"}
```

Use a real serial when available, or host-specific `instance_contains` to distinguish identical hubs. Do not copy a Windows location onto another host. Moving USB ports can change a location selector.

Each monitor has EDID manufacturer/product, optional serial, protocol (`standard` or `lg_alternate`), `inputs`, and optional `readback` mappings. Names such as `dp` and `usbc` are configurable. The Mac example reverses local and remote inputs for complementary operation. Linux assignments depend on cabling.

Defaults coalesce notifications for 200 ms, allow three bounded arrival attempts, and guard resume for 1500 ms. Startup absence does not cause a handoff. Supplied profiles claim the local input at startup only when the selected hub is present. Suspend suppresses USB teardown; wake establishes a fresh baseline. Monitor transports open during transitions, with identity checked before each write. Logs rotate at 256 KiB with one backup.

Both hosts can run complementary profiles. An unplugged or powered-off switch also looks like a departure. Validate rapid switching, sleep, and monitor power-off on the actual setup.

## Build and publish

On the Windows development machine Rust is installed at `C:\Users\peej\.cargo\bin`, without being added to PATH. `scripts/build-windows.ps1` detects Cargo there and the local LLVM-MinGW `llvm-dlltool` for the GNU toolchain:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" --version
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

Windows GNU requires MinGW/LLVM-MinGW `dlltool`; Windows MSVC requires Visual Studio C++ build tools. The script checks formatting, behavior tests, Clippy, and release build, then creates `dist/windows/digital-kvm.exe` with configuration. On Mac/Linux, build as your user:

```sh
cargo fmt -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

[GitHub Actions](https://github.com/Aenima4six2/digital-kvm/actions) runs native Windows x64, Mac ARM64, Linux x64, and Linux ARM64 builds on every push and PR. Native runners test Windows and Mac login installation/reinstallation and Linux boot installation/reinstallation, with monitor writes disabled. Successful builds upload workflow packages; default-branch pushes publish commit-specific prereleases and tag pushes publish releases. Packages contain executable, configuration, installers, documentation, licenses, and metadata. Releases include SHA-256 checksums.

Tests cover transitions, sleep suppression, exact hub selection, complementary profiles, strict configuration, CLI ordering, EDID identities/checksums, and DDC packets. Native builds and service tests do not prove firmware behavior on an untested machine.
