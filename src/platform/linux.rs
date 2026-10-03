//! libudev notifications, logind sleep signals, and kernel i2c-dev DDC.
use super::{Event, EventSink, MonitorControl, MonitorStatus};
use crate::{
    Result,
    config::{MonitorConfig, UsbDevice},
    ddc::{MonitorIdentity, SwitchOutcome, parse_edid, parse_vcp_reply, set_packet},
};
use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    ptr,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};
type P = *mut c_void;
struct Library(P);
impl Library {
    fn open(name: &CStr) -> Result<Self> {
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!(
                "Required Linux library {} is unavailable",
                name.to_string_lossy()
            ));
        }
        Ok(Self(handle))
    }
    fn symbol(&self, name: &CStr) -> Result<P> {
        let address = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if address.is_null() {
            Err(format!(
                "Linux API {} is unavailable",
                name.to_string_lossy()
            ))
        } else {
            Ok(address)
        }
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.0);
        }
    }
}
macro_rules! symbol {
    ($lib:expr,$name:literal,$ty:ty) => {
        unsafe { std::mem::transmute::<P, $ty>($lib.symbol($name)?) }
    };
}
type New = unsafe extern "C" fn() -> P;
type Unary = unsafe extern "C" fn(P) -> P;
type Int = unsafe extern "C" fn(P) -> i32;
type Text = unsafe extern "C" fn(P) -> *const c_char;
type Attr = unsafe extern "C" fn(P, *const c_char) -> *const c_char;
struct Udev {
    context: P,
    library: Library,
    unref: Unary,
    enumerate_new: Unary,
    enumerate_unref: Unary,
    add_subsystem: unsafe extern "C" fn(P, *const c_char) -> i32,
    scan: Int,
    entries: Unary,
    next: Unary,
    entry_name: Text,
    from_path: unsafe extern "C" fn(P, *const c_char) -> P,
    device_unref: Unary,
    devtype: Text,
    syspath: Text,
    attr: Attr,
    action: Text,
    monitor_new: unsafe extern "C" fn(P, *const c_char) -> P,
    monitor_unref: Unary,
    monitor_filter: unsafe extern "C" fn(P, *const c_char, *const c_char) -> i32,
    monitor_enable: Int,
    monitor_fd: Int,
    receive: Unary,
}
fn text(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}
impl Udev {
    fn load() -> Result<Self> {
        let library = Library::open(c"libudev.so.1")?;
        let new = symbol!(library, c"udev_new", New);
        let context = unsafe { new() };
        if context.is_null() {
            return Err("Cannot create udev context".into());
        }
        // Current libudev exports these stable symbols together.
        (|| {
            Ok(Self {
                context,
                unref: symbol!(library, c"udev_unref", Unary),
                enumerate_new: symbol!(library, c"udev_enumerate_new", Unary),
                enumerate_unref: symbol!(library, c"udev_enumerate_unref", Unary),
                add_subsystem: symbol!(
                    library,
                    c"udev_enumerate_add_match_subsystem",
                    unsafe extern "C" fn(P, *const c_char) -> i32
                ),
                scan: symbol!(library, c"udev_enumerate_scan_devices", Int),
                entries: symbol!(library, c"udev_enumerate_get_list_entry", Unary),
                next: symbol!(library, c"udev_list_entry_get_next", Unary),
                entry_name: symbol!(library, c"udev_list_entry_get_name", Text),
                from_path: symbol!(
                    library,
                    c"udev_device_new_from_syspath",
                    unsafe extern "C" fn(P, *const c_char) -> P
                ),
                device_unref: symbol!(library, c"udev_device_unref", Unary),
                devtype: symbol!(library, c"udev_device_get_devtype", Text),
                syspath: symbol!(library, c"udev_device_get_syspath", Text),
                attr: symbol!(library, c"udev_device_get_sysattr_value", Attr),
                action: symbol!(library, c"udev_device_get_action", Text),
                monitor_new: symbol!(
                    library,
                    c"udev_monitor_new_from_netlink",
                    unsafe extern "C" fn(P, *const c_char) -> P
                ),
                monitor_unref: symbol!(library, c"udev_monitor_unref", Unary),
                monitor_filter: symbol!(
                    library,
                    c"udev_monitor_filter_add_match_subsystem_devtype",
                    unsafe extern "C" fn(P, *const c_char, *const c_char) -> i32
                ),
                monitor_enable: symbol!(library, c"udev_monitor_enable_receiving", Int),
                monitor_fd: symbol!(library, c"udev_monitor_get_fd", Int),
                receive: symbol!(library, c"udev_monitor_receive_device", Unary),
                library,
            })
        })()
    }
    fn device(&self, device: P) -> Option<UsbDevice> {
        if text(unsafe { (self.devtype)(device) }).as_deref() != Some("usb_device") {
            return None;
        }
        let vendor = u16::from_str_radix(
            &text(unsafe { (self.attr)(device, c"idVendor".as_ptr()) })?,
            16,
        )
        .ok()?;
        let product = u16::from_str_radix(
            &text(unsafe { (self.attr)(device, c"idProduct".as_ptr()) })?,
            16,
        )
        .ok()?;
        Some(UsbDevice {
            vendor_id: vendor,
            product_id: product,
            serial: text(unsafe { (self.attr)(device, c"serial".as_ptr()) }).unwrap_or_default(),
            instance: text(unsafe { (self.syspath)(device) })?,
        })
    }
    fn devices(&self) -> Result<Vec<UsbDevice>> {
        let enumeration = unsafe { (self.enumerate_new)(self.context) };
        if enumeration.is_null() {
            return Err("Cannot create USB enumeration".into());
        }
        let result = (|| {
            if unsafe { (self.add_subsystem)(enumeration, c"usb".as_ptr()) } < 0
                || unsafe { (self.scan)(enumeration) } < 0
            {
                return Err("USB enumeration failed".into());
            }
            let mut devices = Vec::new();
            let mut entry = unsafe { (self.entries)(enumeration) };
            while !entry.is_null() {
                let path = unsafe { (self.entry_name)(entry) };
                if !path.is_null() {
                    let device = unsafe { (self.from_path)(self.context, path) };
                    if !device.is_null() {
                        if let Some(value) = self.device(device) {
                            devices.push(value);
                        }
                        unsafe {
                            (self.device_unref)(device);
                        }
                    }
                }
                entry = unsafe { (self.next)(entry) };
            }
            devices.sort_by(|a, b| a.instance.cmp(&b.instance));
            Ok(devices)
        })();
        unsafe {
            (self.enumerate_unref)(enumeration);
        }
        result
    }
}
impl Drop for Udev {
    fn drop(&mut self) {
        unsafe {
            (self.unref)(self.context);
        }
        let _ = &self.library;
    }
}
pub fn usb_devices() -> Result<Vec<UsbDevice>> {
    Udev::load()?.devices()
}

type BusCallback = unsafe extern "C" fn(P, P, P) -> i32;
struct SleepBus {
    bus: P,
    slot: P,
    _library: Library,
    bus_unref: Unary,
    slot_unref: Unary,
    fd: Int,
    events: Int,
    timeout: unsafe extern "C" fn(P, *mut u64) -> i32,
    process: unsafe extern "C" fn(P, *mut P) -> i32,
}
struct SleepContext {
    sink: EventSink,
    read: unsafe extern "C" fn(P, *const c_char, ...) -> i32,
}
unsafe extern "C" fn sleep_signal(message: P, context: P, _: P) -> i32 {
    let context = unsafe { &*(context as *const SleepContext) };
    let mut sleeping = 0i32;
    if unsafe { (context.read)(message, c"b".as_ptr(), &mut sleeping) } > 0 {
        context.sink.emit(if sleeping != 0 {
            Event::Suspend
        } else {
            Event::Resume
        });
    }
    0
}
impl SleepBus {
    fn load(sink: EventSink) -> Result<(Self, Box<SleepContext>)> {
        let library = Library::open(c"libsystemd.so.0")?;
        let open = symbol!(
            library,
            c"sd_bus_open_system",
            unsafe extern "C" fn(*mut P) -> i32
        );
        let mut bus = ptr::null_mut();
        if unsafe { open(&mut bus) } < 0 {
            return Err("Cannot connect to logind's system bus for sleep notifications".into());
        }
        let mut result = Self {
            bus,
            slot: ptr::null_mut(),
            bus_unref: symbol!(library, c"sd_bus_unref", Unary),
            slot_unref: symbol!(library, c"sd_bus_slot_unref", Unary),
            fd: symbol!(library, c"sd_bus_get_fd", Int),
            events: symbol!(library, c"sd_bus_get_events", Int),
            timeout: symbol!(
                library,
                c"sd_bus_get_timeout",
                unsafe extern "C" fn(P, *mut u64) -> i32
            ),
            process: symbol!(
                library,
                c"sd_bus_process",
                unsafe extern "C" fn(P, *mut P) -> i32
            ),
            _library: library,
        };
        let read = symbol!(
            result._library,
            c"sd_bus_message_read",
            unsafe extern "C" fn(P, *const c_char, ...) -> i32
        );
        let add = symbol!(
            result._library,
            c"sd_bus_add_match",
            unsafe extern "C" fn(P, *mut P, *const c_char, BusCallback, P) -> i32
        );
        let mut context = Box::new(SleepContext { sink, read });
        let code = unsafe {
            add(bus,&mut result.slot,c"type='signal',sender='org.freedesktop.login1',interface='org.freedesktop.login1.Manager',member='PrepareForSleep'".as_ptr(),sleep_signal,(&mut *context as *mut SleepContext).cast())
        };
        if code < 0 {
            return Err(format!(
                "Cannot subscribe to logind sleep notifications: {code}"
            ));
        }
        Ok((result, context))
    }
    fn timeout_ms(&self) -> i32 {
        let mut micros = u64::MAX;
        if unsafe { (self.timeout)(self.bus, &mut micros) } < 0 || micros == u64::MAX {
            return -1;
        }
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now);
        }
        let now = (now.tv_sec as u64).saturating_mul(1_000_000) + (now.tv_nsec as u64) / 1000;
        micros
            .saturating_sub(now)
            .div_ceil(1000)
            .min(i32::MAX as u64) as i32
    }
}
impl Drop for SleepBus {
    fn drop(&mut self) {
        unsafe {
            (self.slot_unref)(self.slot);
            (self.bus_unref)(self.bus);
        }
    }
}
pub struct Watcher {
    stop: OwnedFd,
    worker: Option<JoinHandle<()>>,
}
impl Watcher {
    pub fn start(sink: EventSink) -> Result<Self> {
        let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGINT);
            libc::sigaddset(&mut signals, libc::SIGTERM);
        }
        if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, ptr::null_mut()) } != 0 {
            return Err("Cannot register termination signals".into());
        }
        let signal_fd =
            unsafe { libc::signalfd(-1, &signals, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        let stop_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if signal_fd < 0 || stop_fd < 0 {
            if signal_fd >= 0 {
                unsafe {
                    libc::close(signal_fd);
                }
            }
            if stop_fd >= 0 {
                unsafe {
                    libc::close(stop_fd);
                }
            }
            return Err("Cannot create Linux notification descriptors".into());
        }
        let signal_fd = unsafe { OwnedFd::from_raw_fd(signal_fd) };
        let stop = unsafe { OwnedFd::from_raw_fd(stop_fd) };
        let worker_stop = stop.try_clone().map_err(|e| e.to_string())?;
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let setup = (|| -> Result<_> {
                let udev = Udev::load()?;
                let monitor = unsafe { (udev.monitor_new)(udev.context, c"udev".as_ptr()) };
                if monitor.is_null() {
                    return Err("Cannot create USB monitor".into());
                }
                if unsafe {
                    (udev.monitor_filter)(monitor, c"usb".as_ptr(), c"usb_device".as_ptr())
                } < 0
                    || unsafe { (udev.monitor_enable)(monitor) } < 0
                {
                    unsafe {
                        (udev.monitor_unref)(monitor);
                    }
                    return Err("Cannot subscribe to USB events".into());
                }
                let devices = udev.devices();
                let power = SleepBus::load(sink.clone());
                match (devices, power) {
                    (Ok(devices), Ok((bus, context))) => Ok((udev, monitor, devices, bus, context)),
                    (Err(error), _) | (_, Err(error)) => {
                        unsafe {
                            (udev.monitor_unref)(monitor);
                        }
                        Err(error)
                    }
                }
            })();
            let (udev, monitor, devices, bus, _sleep_context) = match setup {
                Ok(value) => value,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let mut devices: HashMap<_, _> = devices
                .into_iter()
                .map(|d| (d.instance.clone(), d))
                .collect();
            let _ = ready_tx.send(Ok(()));
            loop {
                let mut bus_ok = true;
                loop {
                    let code = unsafe { (bus.process)(bus.bus, ptr::null_mut()) };
                    if code < 0 {
                        bus_ok = false;
                        break;
                    }
                    if code == 0 {
                        break;
                    }
                }
                if !bus_ok {
                    sink.emit(Event::Stop);
                    break;
                }
                let mut fds = [
                    libc::pollfd {
                        fd: unsafe { (udev.monitor_fd)(monitor) },
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: worker_stop.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: signal_fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: unsafe { (bus.fd)(bus.bus) },
                        events: unsafe { (bus.events)(bus.bus) } as i16,
                        revents: 0,
                    },
                ];
                let code = unsafe {
                    libc::poll(
                        fds.as_mut_ptr(),
                        fds.len() as libc::nfds_t,
                        bus.timeout_ms(),
                    )
                };
                if code < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    sink.emit(Event::Stop);
                    break;
                }
                if fds[1].revents != 0 {
                    break;
                }
                if fds[2].revents != 0 {
                    sink.emit(Event::Stop);
                    break;
                }
                if fds[0].revents & libc::POLLIN != 0 {
                    loop {
                        let device = unsafe { (udev.receive)(monitor) };
                        if device.is_null() {
                            break;
                        }
                        let action = text(unsafe { (udev.action)(device) }).unwrap_or_default();
                        let path = text(unsafe { (udev.syspath)(device) }).unwrap_or_default();
                        let value = if action == "remove" {
                            devices.remove(&path)
                        } else {
                            let value = udev.device(device);
                            if let Some(d) = &value {
                                devices.insert(path, d.clone());
                            }
                            value
                        };
                        if (action == "add" || action == "remove")
                            && value.as_ref().is_some_and(|d| {
                                sink.selector.as_ref().is_none_or(|s| s.matches(d))
                            })
                        {
                            sink.emit(Event::UsbChanged);
                        }
                        unsafe {
                            (udev.device_unref)(device);
                        }
                    }
                }
            }
            unsafe {
                (udev.monitor_unref)(monitor);
            }
        });
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        let value = 1u64;
        unsafe {
            libc::write(self.stop.as_raw_fd(), (&value as *const u64).cast(), 8);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub struct InstanceLock(File);
impl InstanceLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Another instance is running for this configuration".into());
        }
        Ok(Self(file))
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
pub fn detach_console() {} // systemd owns the descriptors

struct Binding {
    identity: MonitorIdentity,
    connector: PathBuf,
    bus: Option<PathBuf>,
}
fn scan() -> Result<Vec<Binding>> {
    let entries =
        fs::read_dir("/sys/class/drm").map_err(|e| format!("DRM connectors unavailable: {e}"))?;
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let connector = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("card")
            || !name.contains('-')
            || name.contains("-eDP-")
            || name.contains("-LVDS-")
        {
            continue;
        }
        let Ok(bytes) = fs::read(connector.join("edid")) else {
            continue;
        };
        let Ok(identity) = parse_edid(&bytes, format!("linux-drm:{name}")) else {
            continue;
        };
        let bus = fs::canonicalize(connector.join("ddc"))
            .ok()
            .and_then(|p| p.file_name().map(|n| Path::new("/dev").join(n)))
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("i2c-"))
            })
            .or_else(|| {
                fs::read_dir(&connector).ok().and_then(|entries| {
                    entries
                        .flatten()
                        .find(|e| e.file_name().to_string_lossy().starts_with("i2c-"))
                        .map(|e| Path::new("/dev").join(e.file_name()))
                })
            });
        result.push(Binding {
            identity,
            connector,
            bus,
        });
    }
    result.sort_by(|a, b| a.identity.transport.cmp(&b.identity.transport));
    Ok(result)
}
fn open_bus(binding: &Binding, config: &MonitorConfig) -> Result<File> {
    let bytes = fs::read(binding.connector.join("edid")).map_err(|e| e.to_string())?;
    if !parse_edid(&bytes, binding.identity.transport.clone())?.matches(config) {
        return Err("Monitor identity changed; refusing DDC command".into());
    }
    let path = binding
        .bus
        .as_ref()
        .ok_or("GPU driver does not expose this connector's DDC adapter in sysfs")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| {
            format!(
                "Cannot open {}: {e}. Run the Linux installer's DDC permission setup.",
                path.display()
            )
        })?;
    // Linux uses the 7-bit slave address. The DDC packet checksum uses 0x6e.
    if unsafe { libc::ioctl(file.as_raw_fd(), 0x0703, 0x37 as libc::c_ulong) } < 0 {
        return Err(format!(
            "I2C_SLAVE failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}
pub struct MonitorBackend {
    bindings: Vec<Binding>,
}
impl MonitorBackend {
    pub fn new(_: &[MonitorConfig]) -> Result<Self> {
        Ok(Self { bindings: scan()? })
    }
    fn binding(&self, config: &MonitorConfig) -> Result<&Binding> {
        let mut matches = self.bindings.iter().filter(|b| b.identity.matches(config));
        let first = matches
            .next()
            .ok_or("Configured monitor is not reachable through Linux DRM")?;
        if matches.next().is_some() {
            return Err("Multiple monitors match; configure an EDID serial".into());
        }
        Ok(first)
    }
    pub fn current_input(&self, config: &MonitorConfig) -> Option<u16> {
        let binding = self.binding(config).ok()?;
        let mut file = open_bus(binding, config).ok()?;
        let request = [0x51, 0x82, 1, 0x60, 0x6e ^ 0x51 ^ 0x82 ^ 1 ^ 0x60];
        file.write_all(&request).ok()?;
        thread::sleep(Duration::from_millis(60));
        let mut reply = [0u8; 11];
        file.read_exact(&mut reply).ok()?;
        parse_vcp_reply(&reply, 0x60)
    }
    pub fn set_input_force(
        &mut self,
        config: &MonitorConfig,
        input: &str,
        force: bool,
    ) -> Result<SwitchOutcome> {
        let value = *config
            .inputs
            .get(input)
            .ok_or_else(|| format!("Unknown input {input}"))?;
        let expected = config.readback.get(input).copied();
        if !force && expected.is_some() && self.current_input(config) == expected {
            return Ok(SwitchOutcome {
                monitor: self.binding(config)?.identity.clone(),
                input: input.into(),
                value,
                already_selected: true,
                verified: true,
            });
        }
        let binding = self.binding(config)?;
        let mut file = open_bus(binding, config)?;
        file.write_all(&set_packet(
            config.protocol.source(),
            config.protocol.vcp(),
            value,
        ))
        .map_err(|e| format!("DDC write failed: {e}"))?;
        let identity = binding.identity.clone();
        thread::sleep(Duration::from_millis(150));
        let verified = expected.is_some() && self.current_input(config) == expected;
        Ok(SwitchOutcome {
            monitor: identity,
            input: input.into(),
            value,
            already_selected: false,
            verified,
        })
    }
}
impl MonitorControl for MonitorBackend {
    fn identities(&self) -> Vec<MonitorIdentity> {
        self.bindings.iter().map(|b| b.identity.clone()).collect()
    }
    fn set_input(&mut self, config: &MonitorConfig, input: &str) -> Result<SwitchOutcome> {
        self.set_input_force(config, input, true)
    }
}
pub fn monitors() -> Result<Vec<MonitorIdentity>> {
    Ok(MonitorBackend::new(&[])?.identities())
}
pub fn full_monitor_status(config: &MonitorConfig) -> MonitorStatus {
    match MonitorBackend::new(std::slice::from_ref(config)) {
        Ok(backend) => {
            let value = backend.current_input(config);
            MonitorStatus {
                current_input: value,
                capabilities: None,
                error: if value.is_none() {
                    Some("Cannot read input: check DDC adapter, permissions, cable, and monitor power".into())
                } else {
                    None
                },
            }
        }
        Err(error) => MonitorStatus {
            current_input: None,
            capabilities: None,
            error: Some(error),
        },
    }
}
