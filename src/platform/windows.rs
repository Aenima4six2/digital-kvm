use super::{Event, EventSink, MonitorControl, MonitorStatus};
use crate::{
    Result,
    config::{MonitorConfig, Protocol, UsbDevice, usb_from_windows_path},
    ddc::{MonitorIdentity, SwitchOutcome, parse_edid, set_packet},
};
use std::{
    ffi::{c_char, c_void},
    mem::{self, size_of},
    path::Path,
    ptr,
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}
const USB_GUID: Guid = Guid {
    a: 0xa5dcbf10,
    b: 0x6530,
    c: 0x11d2,
    d: [0x90, 0x1f, 0, 0xc0, 0x4f, 0xb9, 0x51, 0xed],
};
const HUB_GUID: Guid = Guid {
    a: 0xf18a0e88,
    b: 0xc30c,
    c: 0x11d0,
    d: [0x88, 0x15, 0x00, 0xa0, 0xc9, 0x06, 0xbe, 0xd8],
};
// cfgmgr32's largest filter union member is WCHAR InstanceId[MAX_DEVICE_ID_LEN].
#[repr(C)]
struct NotifyFilter {
    size: u32,
    flags: u32,
    kind: u32,
    reserved: u32,
    data: [u64; 50],
}
type DeviceCallback =
    unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *const u8, u32) -> u32;
#[link(name = "cfgmgr32", kind = "raw-dylib")]
unsafe extern "system" {
    fn CM_Register_Notification(
        filter: *const NotifyFilter,
        context: *mut c_void,
        callback: DeviceCallback,
        handle: *mut *mut c_void,
    ) -> u32;
    fn CM_Unregister_Notification(handle: *mut c_void) -> u32;
    fn CM_Get_Device_Interface_List_SizeW(
        size: *mut u32,
        class: *const Guid,
        device: *const u16,
        flags: u32,
    ) -> u32;
    fn CM_Get_Device_Interface_ListW(
        class: *const Guid,
        device: *const u16,
        buffer: *mut u16,
        size: u32,
        flags: u32,
    ) -> u32;
}
#[repr(C)]
struct PowerRecipient {
    callback: unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> u32,
    context: *mut c_void,
}
#[link(name = "powrprof", kind = "raw-dylib")]
unsafe extern "system" {
    fn PowerRegisterSuspendResumeNotification(
        flags: u32,
        recipient: *const PowerRecipient,
        handle: *mut *mut c_void,
    ) -> u32;
    fn PowerUnregisterSuspendResumeNotification(handle: *mut c_void) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(path: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> i32;
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        attributes: *const c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn FreeConsole() -> i32;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn from_wide(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|x| *x == 0).unwrap_or(s.len())])
}

pub fn usb_devices() -> Result<Vec<UsbDevice>> {
    let mut devices = interface_devices(&USB_GUID)?;
    devices.extend(interface_devices(&HUB_GUID)?);
    devices.sort_by_key(|x| x.instance.to_ascii_lowercase());
    devices.dedup_by(|a, b| a.instance.eq_ignore_ascii_case(&b.instance));
    Ok(devices)
}
fn interface_devices(class: &Guid) -> Result<Vec<UsbDevice>> {
    // The topology can change between the two calls. Retry buffer resizing only
    // on an actual event; this is never a steady-state polling loop.
    for _ in 0..4 {
        let mut count = 0;
        let status =
            unsafe { CM_Get_Device_Interface_List_SizeW(&mut count, class, ptr::null(), 0) };
        if status != 0 {
            return Err(format!("USB enumeration size failed: CM {status}"));
        }
        let mut buffer = vec![0u16; count as usize];
        let status = unsafe {
            CM_Get_Device_Interface_ListW(class, ptr::null(), buffer.as_mut_ptr(), count, 0)
        };
        if status == 26 {
            continue;
        } // CR_BUFFER_SMALL
        if status != 0 {
            return Err(format!("USB enumeration failed: CM {status}"));
        }
        let mut devices: Vec<_> = buffer
            .split(|x| *x == 0)
            .filter(|x| !x.is_empty())
            .filter_map(|x| usb_from_windows_path(&String::from_utf16_lossy(x)))
            .collect();
        devices.sort_by_key(|x| x.instance.to_ascii_lowercase());
        devices.dedup_by(|a, b| a.instance.eq_ignore_ascii_case(&b.instance));
        return Ok(devices);
    }
    Err("USB topology kept changing during enumeration".into())
}

unsafe extern "system" fn device_callback(
    _: *mut c_void,
    context: *mut c_void,
    action: u32,
    data: *const u8,
    size: u32,
) -> u32 {
    if context.is_null() || data.is_null() || size < 26 || action > 1 {
        return 0;
    }
    let sink = unsafe { &*(context as *const EventSink) };
    let kind = unsafe { ptr::read_unaligned(data as *const u32) };
    if kind != 0 {
        return 0;
    }
    let chars =
        unsafe { std::slice::from_raw_parts(data.add(24) as *const u16, (size as usize - 24) / 2) };
    if let Some(device) = usb_from_windows_path(&from_wide(chars))
        && sink.selector.as_ref().is_none_or(|s| s.matches(&device))
    {
        sink.emit(Event::UsbChanged);
    }
    0
}
unsafe extern "system" fn power_callback(context: *mut c_void, kind: u32, _: *mut c_void) -> u32 {
    if !context.is_null() {
        let sink = unsafe { &*(context as *const EventSink) };
        match kind {
            4 => sink.emit(Event::Suspend),
            7 | 18 => sink.emit(Event::Resume),
            _ => {}
        }
    }
    0
}
static STOP_SINK: OnceLock<Mutex<Option<EventSink>>> = OnceLock::new();
pub(super) fn emit_service_event(event: Event) {
    if let Some(lock) = STOP_SINK.get()
        && let Ok(sink) = lock.lock()
        && let Some(sink) = sink.as_ref()
    {
        sink.emit(event);
    }
}
unsafe extern "system" fn console_callback(kind: u32) -> i32 {
    if matches!(kind, 0 | 1 | 2 | 5 | 6) {
        if let Some(lock) = STOP_SINK.get()
            && let Ok(guard) = lock.lock()
            && let Some(sink) = guard.as_ref()
        {
            sink.emit(Event::Stop);
        }
        return 1;
    }
    0
}
pub struct Watcher {
    devices: Vec<*mut c_void>,
    power: *mut c_void,
    _recipient: Box<PowerRecipient>,
    _sink: Box<EventSink>,
}
impl Watcher {
    pub fn start(sink: EventSink) -> Result<Self> {
        let mut sink = Box::new(sink);
        let context = (&mut *sink as *mut EventSink).cast();
        let mut filter = NotifyFilter {
            size: size_of::<NotifyFilter>() as u32,
            flags: 0,
            kind: 0,
            reserved: 0,
            data: [0; 50],
        };
        let mut devices = Vec::new();
        for class in [&USB_GUID, &HUB_GUID] {
            unsafe {
                ptr::copy_nonoverlapping(
                    (class as *const Guid).cast::<u8>(),
                    filter.data.as_mut_ptr().cast(),
                    size_of::<Guid>(),
                );
            }
            let mut device = ptr::null_mut();
            let status =
                unsafe { CM_Register_Notification(&filter, context, device_callback, &mut device) };
            if status != 0 {
                for registered in devices {
                    unsafe {
                        CM_Unregister_Notification(registered);
                    }
                }
                return Err(format!("USB notification registration failed: CM {status}"));
            }
            devices.push(device);
        }
        let recipient = Box::new(PowerRecipient {
            callback: power_callback,
            context,
        });
        let mut power = ptr::null_mut();
        let status = unsafe { PowerRegisterSuspendResumeNotification(2, &*recipient, &mut power) };
        if status != 0 {
            unsafe {
                for device in devices {
                    CM_Unregister_Notification(device);
                }
            }
            return Err(format!(
                "Power notification registration failed: Windows {status}"
            ));
        }
        *STOP_SINK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|e| e.to_string())? = Some((*sink).clone());
        unsafe {
            SetConsoleCtrlHandler(Some(console_callback), 1);
        }
        Ok(Self {
            devices,
            power,
            _recipient: recipient,
            _sink: sink,
        })
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            for device in &self.devices {
                CM_Unregister_Notification(*device);
            }
            PowerUnregisterSuspendResumeNotification(self.power);
            SetConsoleCtrlHandler(Some(console_callback), 0);
        }
        if let Some(lock) = STOP_SINK.get()
            && let Ok(mut guard) = lock.lock()
        {
            *guard = None;
        }
    }
}

pub struct InstanceLock(*mut c_void);
impl InstanceLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        // Share mode zero makes the handle the lock. Process exit releases it;
        // the harmless empty file can remain without creating a stale lock.
        let name = wide(&path.to_string_lossy());
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                0xc0000000,
                0,
                ptr::null(),
                4,
                0x80,
                ptr::null_mut(),
            )
        };
        if handle as isize == -1 {
            return Err(format!(
                "Another instance is running, or {} is not writable: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(handle))
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub fn detach_console() {
    unsafe {
        FreeConsole();
    }
}

#[repr(C)]
struct I2cInfo {
    version: u32,
    mask: u32,
    ddc: u8,
    address: u8,
    reg: *mut u8,
    reg_size: u32,
    data: *mut u8,
    size: u32,
    speed: u32,
    speed_khz: u32,
    port: u8,
    port_set: u32,
}
#[repr(C)]
struct Edid {
    version: u32,
    data: [u8; 256],
    size: u32,
    id: u32,
    offset: u32,
}
type NvInit = unsafe extern "C" fn() -> i32;
type NvEnum = unsafe extern "C" fn(*mut *mut c_void, *mut u32) -> i32;
type NvOutputs = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
type NvEdid = unsafe extern "C" fn(*mut c_void, u32, *mut Edid) -> i32;
type NvWrite = unsafe extern "C" fn(*mut c_void, *mut I2cInfo) -> i32;
struct NvApi {
    module: *mut c_void,
    unload: NvInit,
    enumerate: NvEnum,
    outputs: NvOutputs,
    edid: NvEdid,
    write: NvWrite,
}
impl NvApi {
    fn load() -> Result<Self> {
        let module =
            unsafe { LoadLibraryExW(wide("nvapi64.dll").as_ptr(), ptr::null_mut(), 0x800) }; // System32 only
        if module.is_null() {
            return Err(format!(
                "NVIDIA driver API unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        let result = (|| {
            let symbol = unsafe { GetProcAddress(module, c"nvapi_QueryInterface".as_ptr()) };
            if symbol.is_null() {
                return Err("NVIDIA QueryInterface is unavailable".into());
            }
            let query: unsafe extern "C" fn(u32) -> *mut c_void = unsafe { mem::transmute(symbol) };
            let resolve = |id| -> Result<*mut c_void> {
                let p = unsafe { query(id) };
                if p.is_null() {
                    Err(format!("NVIDIA function {id:#x} is unavailable"))
                } else {
                    Ok(p)
                }
            };
            let init: NvInit = unsafe { mem::transmute(resolve(0x0150e828)?) };
            let unload = unsafe { mem::transmute::<*mut c_void, NvInit>(resolve(0xd22bdd7e)?) };
            let enumerate = unsafe { mem::transmute::<*mut c_void, NvEnum>(resolve(0xe5ac921f)?) };
            let outputs = unsafe { mem::transmute::<*mut c_void, NvOutputs>(resolve(0x7d554f8e)?) }; // all physical connectors
            let edid = unsafe { mem::transmute::<*mut c_void, NvEdid>(resolve(0x37d32e69)?) };
            let write = unsafe { mem::transmute::<*mut c_void, NvWrite>(resolve(0xe812eb07)?) };
            let status = unsafe { init() };
            if status != 0 {
                return Err(format!("NVIDIA initialization failed: {status}"));
            }
            Ok(Self {
                module,
                unload,
                enumerate,
                outputs,
                edid,
                write,
            })
        })();
        if result.is_err() {
            unsafe {
                FreeLibrary(module);
            }
        }
        result
    }
    fn identity(&self, gpu: *mut c_void, mask: u32) -> Result<MonitorIdentity> {
        let mut edid = Edid {
            version: (3 << 16) | size_of::<Edid>() as u32,
            data: [0; 256],
            size: 0,
            id: 0,
            offset: 0,
        };
        let status = unsafe { (self.edid)(gpu, mask, &mut edid) };
        if status != 0 {
            return Err(format!(
                "Monitor EDID unavailable at output {mask:#x}: NVIDIA {status}"
            ));
        }
        parse_edid(&edid.data, format!("nvidia:{mask:#x}"))
    }
    fn scan(&self) -> Result<Vec<Binding>> {
        let mut gpus = [ptr::null_mut(); 64];
        let mut count = 0;
        let status = unsafe { (self.enumerate)(gpus.as_mut_ptr(), &mut count) };
        if status != 0 || count > 64 {
            return Err(format!("NVIDIA GPU enumeration failed: {status}"));
        }
        let mut bindings = Vec::new();
        for gpu in gpus.into_iter().take(count as usize) {
            let mut mask = 0;
            if unsafe { (self.outputs)(gpu, &mut mask) } != 0 {
                continue;
            }
            for bit in 0..32 {
                let output = 1u32 << bit;
                if mask & output != 0
                    && let Ok(identity) = self.identity(gpu, output)
                {
                    bindings.push(Binding {
                        gpu,
                        mask: output,
                        identity,
                    });
                }
            }
        }
        Ok(bindings)
    }
}
impl Drop for NvApi {
    fn drop(&mut self) {
        unsafe {
            (self.unload)();
            FreeLibrary(self.module);
        }
    }
}
struct Binding {
    gpu: *mut c_void,
    mask: u32,
    identity: MonitorIdentity,
}
pub struct MonitorBackend {
    api: NvApi,
    bindings: Vec<Binding>,
}
impl MonitorBackend {
    pub fn new(_: &[MonitorConfig]) -> Result<Self> {
        let api = NvApi::load()?;
        let bindings = api.scan()?;
        Ok(Self { api, bindings })
    }
    pub fn current_input(&self, config: &MonitorConfig) -> Option<u16> {
        monitor_status(config).current_input
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
        let current = self.current_input(config);
        if !force && expected.is_some() && current == expected {
            let matches: Vec<_> = self
                .bindings
                .iter()
                .filter(|b| b.identity.matches(config))
                .collect();
            if matches.len() == 1 {
                return Ok(SwitchOutcome {
                    monitor: matches[0].identity.clone(),
                    input: input.into(),
                    value,
                    already_selected: true,
                    verified: true,
                });
            }
        }
        if !self.bindings.iter().any(|b| b.identity.matches(config)) {
            self.bindings = self.api.scan()?;
        }
        let matches: Vec<_> = self
            .bindings
            .iter()
            .filter(|b| b.identity.matches(config))
            .collect();
        if matches.len() != 1 {
            return Err(format!(
                "Expected one matching monitor; found {}. Configure its EDID serial.",
                matches.len()
            ));
        }
        let binding = matches[0];
        // Never broadcast writes to arbitrary outputs. Revalidate the monitor
        // before every command so an unplug/replug cannot target another panel.
        let identity = self.api.identity(binding.gpu, binding.mask)?;
        if !identity.matches(config) {
            return Err("Monitor identity changed; refusing to send an input command".into());
        }
        match config.protocol {
            Protocol::Standard => {
                highlevel_set(config, value)?;
            }
            Protocol::LgAlternate => {
                let mut bytes = set_packet(config.protocol.source(), config.protocol.vcp(), value);
                let mut info = I2cInfo {
                    version: (3 << 16) | size_of::<I2cInfo>() as u32,
                    mask: binding.mask,
                    ddc: 1,
                    address: 0x6e,
                    reg: ptr::null_mut(),
                    reg_size: 0,
                    data: bytes.as_mut_ptr(),
                    size: bytes.len() as u32,
                    speed: 0xffff,
                    speed_khz: 0,
                    port: 0,
                    port_set: 0,
                };
                let status = unsafe { (self.api.write)(binding.gpu, &mut info) };
                if status != 0 {
                    return Err(format!("Monitor command failed: NVIDIA {status}"));
                }
            }
        }
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
        // Input readback on an inactive connection can describe the old link.
        // Ownership transitions must actually send the requested command.
        self.set_input_force(config, input, true)
    }
}
pub fn monitors() -> Result<Vec<MonitorIdentity>> {
    Ok(MonitorBackend::new(&[])?.identities())
}

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
    device: [u16; 32],
}
#[repr(C)]
struct DisplayDevice {
    size: u32,
    name: [u16; 32],
    description: [u16; 128],
    flags: u32,
    id: [u16; 128],
    key: [u16; 128],
}
#[repr(C)]
struct PhysicalMonitor {
    handle: *mut c_void,
    description: [u16; 128],
}
type EnumCallback = unsafe extern "system" fn(*mut c_void, *mut c_void, *const Rect, isize) -> i32;
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumDisplayMonitors(
        dc: *mut c_void,
        clip: *const Rect,
        callback: EnumCallback,
        context: isize,
    ) -> i32;
    fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
    fn EnumDisplayDevicesW(
        device: *const u16,
        index: u32,
        result: *mut DisplayDevice,
        flags: u32,
    ) -> i32;
}
#[link(name = "dxva2", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetNumberOfPhysicalMonitorsFromHMONITOR(monitor: *mut c_void, count: *mut u32) -> i32;
    fn GetPhysicalMonitorsFromHMONITOR(
        monitor: *mut c_void,
        count: u32,
        result: *mut PhysicalMonitor,
    ) -> i32;
    fn DestroyPhysicalMonitors(count: u32, monitors: *mut PhysicalMonitor) -> i32;
    fn GetVCPFeatureAndVCPFeatureReply(
        monitor: *mut c_void,
        code: u8,
        kind: *mut u32,
        current: *mut u32,
        max: *mut u32,
    ) -> i32;
    fn SetVCPFeature(monitor: *mut c_void, code: u8, value: u32) -> i32;
    fn GetCapabilitiesStringLength(monitor: *mut c_void, length: *mut u32) -> i32;
    fn CapabilitiesRequestAndCapabilitiesReply(
        monitor: *mut c_void,
        buffer: *mut u8,
        length: u32,
    ) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: *mut c_void,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
}

struct ProbeContext<'a> {
    config: &'a MonitorConfig,
    status: MonitorStatus,
    set: Option<u16>,
    include_capabilities: bool,
    matches: u32,
}
unsafe extern "system" fn monitor_callback(
    handle: *mut c_void,
    _: *mut c_void,
    _: *const Rect,
    context: isize,
) -> i32 {
    let context = unsafe { &mut *(context as *mut ProbeContext<'_>) };
    let mut info: MonitorInfo = unsafe { mem::zeroed() };
    info.size = size_of::<MonitorInfo>() as u32;
    if unsafe { GetMonitorInfoW(handle, &mut info) } == 0 {
        return 1;
    }
    let mut device: DisplayDevice = unsafe { mem::zeroed() };
    device.size = size_of::<DisplayDevice>() as u32;
    if unsafe { EnumDisplayDevicesW(info.device.as_ptr(), 0, &mut device, 1) } == 0 {
        return 1;
    }
    let path = from_wide(&device.id);
    let parts: Vec<_> = path.split('#').collect();
    if parts.len() < 3 {
        return 1;
    }
    let key = wide(&format!(
        "SYSTEM\\CurrentControlSet\\Enum\\DISPLAY\\{}\\{}\\Device Parameters",
        parts[1], parts[2]
    ));
    let mut edid = [0u8; 1024];
    let mut length = edid.len() as u32;
    let hklm = 0x80000002u32 as i32 as isize as *mut c_void;
    if unsafe {
        RegGetValueW(
            hklm,
            key.as_ptr(),
            wide("EDID").as_ptr(),
            8,
            ptr::null_mut(),
            edid.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
    {
        return 1;
    }
    if !parse_edid(&edid[..length as usize], path)
        .is_ok_and(|identity| identity.matches(context.config))
    {
        return 1;
    }
    context.matches += 1;
    let mut count = 0;
    if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(handle, &mut count) } == 0
        || count == 0
        || count > 16
    {
        return 1;
    }
    let mut physical: Vec<PhysicalMonitor> = (0..count).map(|_| unsafe { mem::zeroed() }).collect();
    if unsafe { GetPhysicalMonitorsFromHMONITOR(handle, count, physical.as_mut_ptr()) } == 0 {
        return 1;
    }
    for monitor in &physical {
        if let Some(value) = context.set {
            if unsafe { SetVCPFeature(monitor.handle, 0x60, value as u32) } == 0 {
                context.status.error = Some(format!(
                    "SetVCPFeature failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        } else {
            let (mut kind, mut current, mut max) = (0, 0, 0);
            if unsafe {
                GetVCPFeatureAndVCPFeatureReply(
                    monitor.handle,
                    0x60,
                    &mut kind,
                    &mut current,
                    &mut max,
                )
            } != 0
            {
                context.status.current_input = Some(current as u16);
            } else {
                context.status.error = Some(format!(
                    "Input read failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if context.include_capabilities {
                let mut len = 0;
                if unsafe { GetCapabilitiesStringLength(monitor.handle, &mut len) } != 0
                    && len > 0
                    && len < 65536
                {
                    let mut bytes = vec![0; len as usize];
                    if unsafe {
                        CapabilitiesRequestAndCapabilitiesReply(
                            monitor.handle,
                            bytes.as_mut_ptr(),
                            len,
                        )
                    } != 0
                    {
                        context.status.capabilities = Some(
                            String::from_utf8_lossy(&bytes)
                                .trim_end_matches('\0')
                                .into(),
                        );
                    }
                }
            }
        }
    }
    unsafe {
        DestroyPhysicalMonitors(count, physical.as_mut_ptr());
    }
    1
}
fn probe(config: &MonitorConfig, set: Option<u16>, capabilities: bool) -> ProbeContext<'_> {
    let mut context = ProbeContext {
        config,
        status: MonitorStatus::default(),
        set,
        include_capabilities: capabilities,
        matches: 0,
    };
    unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            monitor_callback,
            (&mut context as *mut ProbeContext<'_>) as isize,
        );
    }
    if context.matches == 0 {
        context.status.error = Some("Monitor is not currently enumerated by Windows".into());
    }
    context
}
pub fn monitor_status(config: &MonitorConfig) -> MonitorStatus {
    probe(config, None, false).status
}
pub fn full_monitor_status(config: &MonitorConfig) -> MonitorStatus {
    probe(config, None, true).status
}
fn highlevel_set(config: &MonitorConfig, value: u16) -> Result<()> {
    let result = probe(config, Some(value), false);
    if let Some(error) = result.status.error {
        Err(error)
    } else {
        Ok(())
    }
}
