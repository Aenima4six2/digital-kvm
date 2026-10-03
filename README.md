# Digital KVM

A small Windows and macOS background program that connects an existing USB switch to monitor input selection. Pressing the physical switch moves the peripherals; Digital KVM selects the display input assigned to the computer receiving them.

The same Rust controller runs on both platforms, with native USB and monitor adapters. It blocks on operating-system notifications while idle. It supports multiple explicitly configured monitors and writes bounded JSON event logs.

## Windows quick start

The local build is in `dist/windows/digital-kvm.exe`. Its neighboring `config.json` supplies the default configuration.

```powershell
.\dist\windows\digital-kvm.exe status
.\dist\windows\digital-kvm.exe watch
.\dist\windows\digital-kvm.exe run --dry-run
```

`watch` records all USB arrivals and removals, including hubs, to help choose a trigger. `run --dry-run` records the configured input transitions without sending monitor commands. Press the USB switch to the other computer, wait several seconds, then press it back. The selected receiver should disappear and return, and dry-run should report `usbc` followed by `dp`.

For a bounded monitor test, keep the Mac connected and awake:

```powershell
.\dist\windows\digital-kvm.exe roundtrip --hold-ms 8000
```

This sends the configured remote input, reports its readback after eight seconds, and restores the local input. A separate short-lived restore process also attempts restoration if the testing process is interrupted. Readback must confirm the actual switch; a successful transport call alone is not firmware confirmation. If neither process can reach the inactive port, restore DisplayPort with the monitor's physical controls and use the two-host deployment instead.

After confirming the hardware, run in the foreground or install it for login startup:

```powershell
.\dist\windows\digital-kvm.exe run
powershell -ExecutionPolicy Bypass -File .\scripts\install-windows.ps1
```

The installer copies the executable and configuration to `%LOCALAPPDATA%\DigitalKvm`, adds a current-user login startup entry, and starts a hidden background process. It needs no administrator access. Pass `-NoStart -NoStartup` to stage the files for testing. Reinstalling without `-Config` preserves an existing configuration. Supplying `-Config PATH` intentionally replaces it.

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\stop-windows.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\uninstall-windows.ps1
```

Uninstall disables startup and stops the installed process. It retains the executable, configuration, and logs.

## macOS quick start

Copy this project to the Apple Silicon Mac. With Rust and Apple's command-line build tools available:

```sh
cargo test --locked
cargo build --release --locked
./target/release/digital-kvm devices
./target/release/digital-kvm status --config examples/macos.json
./target/release/digital-kvm run --dry-run --config examples/macos.json
```

After validating the Mac connection:

```sh
sh scripts/install-macos.sh
```

The installer builds and copies the program under `~/Library/Application Support/DigitalKvm` and registers a user LaunchAgent named `local.digital-kvm`. It starts at login and recovers from unexpected exits. To disable it, run `sh scripts/uninstall-macos.sh`. To preserve a customized configuration on reinstall, pass its path as the first argument to `install-macos.sh`.

## Configuration

Use `examples/windows.json` or `examples/macos.json` as the starting point. All USB and monitor IDs are decimal JSON numbers; hexadecimal IDs shown by Windows must be converted. `devices` and `monitors` print matching decimal values. An example USB receiver can therefore be selected as:

```json
{ "vendor_id": 2821, "product_id": 6862, "serial": "W1MPGDD00DC9" }
```

`serial` provides a stable selector across hosts. When a USB device has no real serial, Windows may print an OS-generated location identifier. Leave `serial` unset and use `instance_contains` to distinguish identical devices on a particular host. Windows and macOS location paths differ; do not copy a Windows location filter to the Mac.

Every monitor has its own EDID manufacturer, product, optional serial, protocol, input-value map, and readback map. Use `standard` for normal VCP `0x60` input switching, or `lg_alternate` for LG's alternate command. Names such as `dp` and `usbc` are configuration keys, not hard-coded toggles.

`validate --config PATH` checks a configuration without querying hardware. Unknown fields are rejected to catch typos. Startup absence is not a departure. `reconcile_on_start` defaults to false; set it to true to claim the local input at startup when the configured USB device is present.

## Event handling and resource use

USB callbacks enqueue small messages and return immediately. The controller samples USB presence after a configurable debounce interval, collapses duplicate notifications, and cancels retries when a newer USB event arrives. Departures get one handoff attempt; arrivals get a bounded number of reconciliation attempts. Sleep notifications cancel pending work; wake uses a guard interval before reading presence again. An OS-managed file lock prevents two instances from running with the same configuration.

The Windows adapter enumerates USB device and hub interfaces with Configuration Manager and uses power callbacks without a UI/message-loop window. NVIDIA output identity is checked against EDID before each write. Writes are never broadcast across all display ports. The Mac adapter uses IOKit notifications and a CoreFoundation run loop, and selects its display transport by monitor identity.

Logs default to `%LOCALAPPDATA%\DigitalKvm\digital-kvm.log` or `~/Library/Application Support/DigitalKvm/digital-kvm.log`. `watch` uses `watch.log`. Each is rotated at 256 KiB with one previous file. Input commands explicitly report whether readback verified them or only the transport accepted them.

The initial Windows recorder sample used about 1.1 MiB private memory and 6.3 MiB working set, with 0 seconds measurable CPU during idle observation. This is a local measurement, not a cross-platform performance guarantee. Monitor adapters are created for transitions and dropped afterward to keep the idle footprint small.

Both hosts can run complementary profiles. Duplicate commands select the same destination, but inactive-input DDC availability and rapid-switch timing must be validated on the actual monitor. A USB unplug or hardware power loss can look like a button press; configure a device that moves exclusively with the intended switch.

## Build and checks

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

The build script runs formatting, behavior tests, Clippy, and the release build. Windows GNU Rust additionally needs MinGW `dlltool` or LLVM `llvm-dlltool`; Windows MSVC needs the Visual Studio C++ build tools. The distributed executable does not require the Rust toolchain.

```sh
cargo fmt -- --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
```

Tests cover startup absence, event duplication, bounce coalescing, sleep teardown, complementary host profiles, configuration validation, exact USB matching, monitor identity/checksum validation, and DDC wire packets. Live device notifications, monitor firmware behavior, and macOS private APIs require hardware checks.

## Protocol and API references

- [ddcutil maintainer research on LG input switching](https://github.com/rockowitz/ddcutil/wiki/Switching-input-source-on-LG-monitors)
- [NVIDIA NVAPI I2C documentation](https://docs.nvidia.com/nvapi/group__i2capi.html)
- [m1ddc Apple Silicon implementation](https://github.com/waydabber/m1ddc), including the IOAVService transport and LG alternate packet format
- [Windows Configuration Manager notifications](https://learn.microsoft.com/en-us/windows/win32/api/cfgmgr32/nf-cfgmgr32-cm_register_notification)
- [Apple IOKit device notifications](https://developer.apple.com/documentation/iokit/1514362-ioserviceaddmatchingnotification)
- [Apple sleep and wake notification guidance](https://developer.apple.com/library/archive/qa/qa1340/_index.html)

Hardware research is preserved in `docs/hardware-evidence.json`. This repository implements the published protocol directly; it does not redistribute the Python LG-switch application.
