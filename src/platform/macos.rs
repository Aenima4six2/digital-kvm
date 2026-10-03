//! Native IOKit events and Apple Silicon display DDC. IOAVService and CoreDisplay
//! are private macOS APIs; runtime availability is checked rather than assumed.
use super::{Event, EventSink, MonitorControl, MonitorStatus};
use crate::{
    Result,
    config::{MonitorConfig, UsbDevice},
    ddc::{MonitorIdentity, SwitchOutcome, ioav_get_packet, parse_vcp_reply, set_packet},
};
use std::{
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    path::Path,
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
type CF = *const c_void;
type Io = u32;
const PLANE: &CStr = c"IOService";
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CF);
    fn CFGetTypeID(value: CF) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFStringCreateWithCString(allocator: CF, value: *const c_char, encoding: u32) -> CF;
    fn CFStringGetCString(value: CF, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFNumberGetValue(value: CF, kind: i32, result: *mut c_void) -> u8;
    fn CFDictionaryGetValue(dictionary: CF, key: CF) -> CF;
    fn CFRunLoopGetCurrent() -> CF;
    fn CFRunLoopAddSource(runloop: CF, source: CF, mode: CF);
    fn CFRunLoopRun();
    fn CFRunLoopStop(runloop: CF);
    static kCFRunLoopDefaultMode: CF;
}
type UsbCallback = unsafe extern "C" fn(*mut c_void, Io);
type PowerCallback = unsafe extern "C" fn(*mut c_void, Io, u32, *mut c_void);
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CF;
    fn IOServiceGetMatchingServices(port: u32, matching: CF, iterator: *mut Io) -> i32;
    fn IOIteratorNext(iterator: Io) -> Io;
    fn IOObjectRelease(object: Io) -> i32;
    fn IOObjectConformsTo(object: Io, class: *const c_char) -> u8;
    fn IORegistryGetRootEntry(port: u32) -> Io;
    fn IORegistryEntryGetName(entry: Io, name: *mut c_char) -> i32;
    fn IORegistryEntryGetRegistryEntryID(entry: Io, id: *mut u64) -> i32;
    fn IORegistryEntryGetPath(entry: Io, plane: *const c_char, path: *mut c_char) -> i32;
    fn IORegistryEntryFromPath(port: u32, path: *const c_char) -> Io;
    fn IORegistryEntryCreateCFProperty(entry: Io, key: CF, allocator: CF, options: u32) -> CF;
    fn IORegistryEntrySearchCFProperty(
        entry: Io,
        plane: *const c_char,
        key: CF,
        allocator: CF,
        options: u32,
    ) -> CF;
    fn IORegistryEntryCreateIterator(
        entry: Io,
        plane: *const c_char,
        options: u32,
        iterator: *mut Io,
    ) -> i32;
    fn IORegistryEntryGetParentEntry(entry: Io, plane: *const c_char, parent: *mut Io) -> i32;
    fn IONotificationPortCreate(port: u32) -> *mut c_void;
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> CF;
    fn IONotificationPortDestroy(port: *mut c_void);
    fn IOServiceAddMatchingNotification(
        port: *mut c_void,
        kind: *const c_char,
        matching: CF,
        callback: UsbCallback,
        context: *mut c_void,
        iterator: *mut Io,
    ) -> i32;
    fn IORegisterForSystemPower(
        context: *mut c_void,
        port: *mut *mut c_void,
        callback: PowerCallback,
        notifier: *mut Io,
    ) -> Io;
    fn IOAllowPowerChange(connection: Io, notification: isize) -> i32;
    fn IODeregisterForSystemPower(notifier: *mut Io) -> i32;
    fn IOServiceClose(connection: Io) -> i32;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(max: u32, list: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayVendorNumber(display: u32) -> u32;
    fn CGDisplayModelNumber(display: u32) -> u32;
    fn CGDisplaySerialNumber(display: u32) -> u32;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
}
unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> i32;
    fn flock(fd: i32, operation: i32) -> i32;
    fn pthread_sigmask(how: i32, set: *const u32, old: *mut u32) -> i32;
    fn sigwait(set: *const u32, signal: *mut i32) -> i32;
}
struct OwnedCf(CF);
impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
}
struct OwnedIo(Io);
impl Drop for OwnedIo {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                IOObjectRelease(self.0);
            }
        }
    }
}
fn cf_string(text: &str) -> OwnedCf {
    let text = std::ffi::CString::new(text).unwrap_or_default();
    OwnedCf(unsafe { CFStringCreateWithCString(ptr::null(), text.as_ptr(), 0x08000100) })
}
fn string(value: CF) -> Option<String> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }
    let mut bytes = [0i8; 2048];
    if unsafe { CFStringGetCString(value, bytes.as_mut_ptr(), bytes.len() as isize, 0x08000100) }
        == 0
    {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}
fn number(value: CF) -> Option<u64> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut result = 0i64;
    if unsafe { CFNumberGetValue(value, 4, (&mut result as *mut i64).cast()) } == 0 {
        None
    } else {
        Some(result as u64)
    }
}
fn dictionary_get(dictionary: CF, key: &str) -> CF {
    if dictionary.is_null() || unsafe { CFGetTypeID(dictionary) != CFDictionaryGetTypeID() } {
        return ptr::null();
    }
    let key = cf_string(key);
    unsafe { CFDictionaryGetValue(dictionary, key.0) }
}
fn property(entry: Io, key: &str, recursive: bool) -> OwnedCf {
    let key = cf_string(key);
    OwnedCf(unsafe {
        if recursive {
            IORegistryEntrySearchCFProperty(entry, PLANE.as_ptr(), key.0, ptr::null(), 1)
        } else {
            IORegistryEntryCreateCFProperty(entry, key.0, ptr::null(), 0)
        }
    })
}
fn usb_device(entry: Io) -> Option<(u64, UsbDevice)> {
    let vendor = number(property(entry, "idVendor", false).0)? as u16;
    let product = number(property(entry, "idProduct", false).0)? as u16;
    let serial = string(property(entry, "USB Serial Number", false).0).unwrap_or_default();
    let mut id = 0;
    if unsafe { IORegistryEntryGetRegistryEntryID(entry, &mut id) } != 0 {
        return None;
    }
    let mut path = [0i8; 1024];
    let instance =
        if unsafe { IORegistryEntryGetPath(entry, PLANE.as_ptr(), path.as_mut_ptr()) } == 0 {
            unsafe { CStr::from_ptr(path.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            format!("ioreg:{id}")
        };
    Some((
        id,
        UsbDevice {
            vendor_id: vendor,
            product_id: product,
            serial,
            instance,
        },
    ))
}
pub fn usb_devices() -> Result<Vec<UsbDevice>> {
    let mut iterator = 0;
    let matching = unsafe { IOServiceMatching(c"IOUSBHostDevice".as_ptr()) };
    if matching.is_null() {
        return Err("USB matching dictionary unavailable".into());
    }
    let status = unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator) };
    if status != 0 {
        return Err(format!("USB enumeration failed: IOKit {status:#x}"));
    }
    let iterator = OwnedIo(iterator);
    let mut devices = Vec::new();
    loop {
        let entry = OwnedIo(unsafe { IOIteratorNext(iterator.0) });
        if entry.0 == 0 {
            break;
        }
        if let Some((_, device)) = usb_device(entry.0) {
            devices.push(device);
        }
    }
    devices.sort_by(|a, b| a.instance.cmp(&b.instance));
    Ok(devices)
}
struct WatchContext {
    sink: EventSink,
    devices: Mutex<HashMap<u64, UsbDevice>>,
    initializing: AtomicBool,
    power: Io,
}
unsafe extern "C" fn arrived(context: *mut c_void, iterator: Io) {
    let context = unsafe { &*(context as *const WatchContext) };
    loop {
        let entry = OwnedIo(unsafe { IOIteratorNext(iterator) });
        if entry.0 == 0 {
            break;
        }
        if let Some((id, device)) = usb_device(entry.0) {
            let relevant = context
                .sink
                .selector
                .as_ref()
                .is_none_or(|s| s.matches(&device));
            if let Ok(mut devices) = context.devices.lock() {
                devices.insert(id, device);
            }
            if relevant && !context.initializing.load(Ordering::SeqCst) {
                context.sink.emit(Event::UsbChanged);
            }
        }
    }
}
unsafe extern "C" fn removed(context: *mut c_void, iterator: Io) {
    let context = unsafe { &*(context as *const WatchContext) };
    loop {
        let entry = OwnedIo(unsafe { IOIteratorNext(iterator) });
        if entry.0 == 0 {
            break;
        }
        let mut id = 0;
        unsafe {
            IORegistryEntryGetRegistryEntryID(entry.0, &mut id);
        }
        let device = context
            .devices
            .lock()
            .ok()
            .and_then(|mut devices| devices.remove(&id));
        if device
            .as_ref()
            .is_some_and(|d| context.sink.selector.as_ref().is_none_or(|s| s.matches(d)))
            && !context.initializing.load(Ordering::SeqCst)
        {
            context.sink.emit(Event::UsbChanged);
        }
    }
}
unsafe extern "C" fn power(context: *mut c_void, _: Io, kind: u32, argument: *mut c_void) {
    let context = unsafe { &*(context as *const WatchContext) };
    match kind {
        0xe0000270 => unsafe {
            IOAllowPowerChange(context.power, argument as isize);
        },
        0xe0000280 => {
            context.sink.emit(Event::Suspend);
            unsafe {
                IOAllowPowerChange(context.power, argument as isize);
            }
        }
        0xe0000300 => context.sink.emit(Event::Resume),
        _ => {}
    }
}
pub struct Watcher {
    runloop: usize,
    thread: Option<JoinHandle<()>>,
}
impl Watcher {
    pub fn start(sink: EventSink) -> Result<Self> {
        // sigwait runs on a normal thread; the event sender is never called
        // from an asynchronous POSIX signal handler.
        let signals = (1u32 << (2 - 1)) | (1u32 << (15 - 1));
        if unsafe { pthread_sigmask(1, &signals, ptr::null_mut()) } != 0 {
            return Err("Cannot register termination signals".into());
        }
        let signal_sink = sink.clone();
        thread::spawn(move || {
            let mut signal = 0;
            if unsafe { sigwait(&signals, &mut signal) } == 0 {
                signal_sink.emit(Event::Stop);
            }
        });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<usize>>();
        let worker = thread::spawn(move || {
            let mut context = Box::new(WatchContext {
                sink,
                devices: Mutex::new(HashMap::new()),
                initializing: AtomicBool::new(true),
                power: 0,
            });
            let context_ptr = (&mut *context as *mut WatchContext).cast();
            let port = unsafe { IONotificationPortCreate(0) };
            if port.is_null() {
                let _ = ready_tx.send(Err("USB notification port unavailable".into()));
                return;
            }
            let runloop = unsafe { CFRunLoopGetCurrent() };
            unsafe {
                CFRunLoopAddSource(
                    runloop,
                    IONotificationPortGetRunLoopSource(port),
                    kCFRunLoopDefaultMode,
                );
            }
            let (mut arrival, mut removal, mut notifier) = (0, 0, 0);
            let mut power_port = ptr::null_mut();
            let registration = (|| -> Result<()> {
                let code = unsafe {
                    IOServiceAddMatchingNotification(
                        port,
                        c"IOServiceFirstMatch".as_ptr(),
                        IOServiceMatching(c"IOUSBHostDevice".as_ptr()),
                        arrived,
                        context_ptr,
                        &mut arrival,
                    )
                };
                if code != 0 {
                    return Err(format!("USB arrival registration failed: {code:#x}"));
                }
                unsafe {
                    arrived(context_ptr, arrival);
                }
                let code = unsafe {
                    IOServiceAddMatchingNotification(
                        port,
                        c"IOServiceTerminate".as_ptr(),
                        IOServiceMatching(c"IOUSBHostDevice".as_ptr()),
                        removed,
                        context_ptr,
                        &mut removal,
                    )
                };
                if code != 0 {
                    return Err(format!("USB removal registration failed: {code:#x}"));
                }
                unsafe {
                    removed(context_ptr, removal);
                }
                context.power = unsafe {
                    IORegisterForSystemPower(context_ptr, &mut power_port, power, &mut notifier)
                };
                if context.power == 0 {
                    return Err("Power notification registration failed".into());
                }
                unsafe {
                    CFRunLoopAddSource(
                        runloop,
                        IONotificationPortGetRunLoopSource(power_port),
                        kCFRunLoopDefaultMode,
                    );
                }
                Ok(())
            })();
            if registration.is_ok() {
                context.initializing.store(false, Ordering::SeqCst);
                let _ = ready_tx.send(Ok(runloop as usize));
                unsafe {
                    CFRunLoopRun();
                }
            } else {
                let _ = ready_tx.send(registration.map(|_| 0));
            }
            unsafe {
                if notifier != 0 {
                    IODeregisterForSystemPower(&mut notifier);
                }
                if context.power != 0 {
                    IOServiceClose(context.power);
                }
                if !power_port.is_null() {
                    IONotificationPortDestroy(power_port);
                }
                if arrival != 0 {
                    IOObjectRelease(arrival);
                }
                if removal != 0 {
                    IOObjectRelease(removal);
                }
                IONotificationPortDestroy(port);
            }
        });
        let runloop = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self {
            runloop,
            thread: Some(worker),
        })
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            CFRunLoopStop(self.runloop as CF);
        }
        if let Some(worker) = self.thread.take() {
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
        if unsafe { flock(file.as_raw_fd(), 6) } != 0 {
            return Err("Another instance is running for this configuration".into());
        }
        Ok(Self(file))
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            flock(self.0.as_raw_fd(), 8);
        }
    }
}
pub fn detach_console() {} // launchd owns standard input/output on macOS

type CreateDisplayInfo = unsafe extern "C" fn(u32) -> CF;
type CreateAv = unsafe extern "C" fn(CF, Io) -> CF;
type ReadI2c = unsafe extern "C" fn(CF, u32, u32, *mut c_void, u32) -> i32;
type WriteI2c = unsafe extern "C" fn(CF, u32, u32, *const c_void, u32) -> i32;
struct DisplayApi {
    core_display: *mut c_void,
    info: CreateDisplayInfo,
    create: CreateAv,
    read: ReadI2c,
    write: WriteI2c,
}
impl DisplayApi {
    fn load() -> Result<Self> {
        if !cfg!(target_arch = "aarch64") {
            return Err("Native DDC currently requires an Apple Silicon Mac; Intel Mac DDC is not implemented".into());
        }
        let library = unsafe {
            dlopen(
                c"/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay".as_ptr(),
                1,
            )
        };
        if library.is_null() {
            return Err("CoreDisplay framework unavailable".into());
        }
        let resolve = |handle, name: &CStr| -> Result<*mut c_void> {
            let p = unsafe { dlsym(handle, name.as_ptr()) };
            if p.is_null() {
                Err(format!(
                    "macOS display API {} unavailable",
                    name.to_string_lossy()
                ))
            } else {
                Ok(p)
            }
        };
        let result = (|| {
            let global = -2isize as *mut c_void;
            Ok(Self {
                core_display: library,
                info: unsafe {
                    std::mem::transmute::<*mut c_void, CreateDisplayInfo>(resolve(
                        library,
                        c"CoreDisplay_DisplayCreateInfoDictionary",
                    )?)
                },
                create: unsafe {
                    std::mem::transmute::<*mut c_void, CreateAv>(resolve(
                        global,
                        c"IOAVServiceCreateWithService",
                    )?)
                },
                read: unsafe {
                    std::mem::transmute::<*mut c_void, ReadI2c>(resolve(
                        global,
                        c"IOAVServiceReadI2C",
                    )?)
                },
                write: unsafe {
                    std::mem::transmute::<*mut c_void, WriteI2c>(resolve(
                        global,
                        c"IOAVServiceWriteI2C",
                    )?)
                },
            })
        })();
        if result.is_err() {
            unsafe {
                dlclose(library);
            }
        }
        result
    }
    fn registry_scan(&self) -> Result<Vec<DisplayBinding>> {
        let root = OwnedIo(unsafe { IORegistryGetRootEntry(0) });
        let mut iterator = 0;
        if unsafe { IORegistryEntryCreateIterator(root.0, PLANE.as_ptr(), 1, &mut iterator) } != 0 {
            return Ok(Vec::new());
        }
        let iterator = OwnedIo(iterator);
        let mut identity = None;
        let mut result = Vec::new();
        loop {
            let entry = OwnedIo(unsafe { IOIteratorNext(iterator.0) });
            if entry.0 == 0 {
                break;
            }
            let mut name = [0i8; 128];
            unsafe {
                IORegistryEntryGetName(entry.0, name.as_mut_ptr());
            }
            let name = unsafe { CStr::from_ptr(name.as_ptr()) };
            if unsafe { IOObjectConformsTo(entry.0, c"IOMobileFramebuffer".as_ptr()) } != 0
                || name == c"AppleCLCD2"
                || name == c"IOMobileFramebufferShim"
            {
                identity = None;
                let attributes = property(entry.0, "DisplayAttributes", true);
                let product = dictionary_get(attributes.0, "ProductAttributes");
                let uuid = string(property(entry.0, "EDID UUID", true).0).unwrap_or_default();
                let compact: String = uuid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
                if compact.len() < 8 {
                    continue;
                }
                let Ok(vendor) = u16::from_str_radix(&compact[..4], 16) else {
                    continue;
                };
                let Ok(model_bytes) = u16::from_str_radix(&compact[4..8], 16) else {
                    continue;
                };
                let serial = string(dictionary_get(product, "AlphanumericSerialNumber"))
                    .unwrap_or_else(|| {
                        number(dictionary_get(product, "SerialNumber"))
                            .unwrap_or(0)
                            .to_string()
                    });
                let manufacturer = [10, 5, 0]
                    .iter()
                    .map(|shift| (((vendor >> shift) & 31) as u8 + 64) as char)
                    .collect();
                let mut id = 0;
                unsafe {
                    IORegistryEntryGetRegistryEntryID(entry.0, &mut id);
                }
                identity = Some(MonitorIdentity {
                    manufacturer_id: vendor,
                    manufacturer,
                    product_id: model_bytes.swap_bytes(),
                    serial,
                    name: string(dictionary_get(product, "ProductName"))
                        .unwrap_or_else(|| "External display".into()),
                    transport: format!("apple-ioreg:{id}"),
                });
            } else if name == c"DCPAVServiceProxy"
                && let Some(identity) = identity.as_ref()
            {
                if string(property(entry.0, "Location", true).0).as_deref() != Some("External") {
                    continue;
                }
                let av = OwnedCf(unsafe { (self.create)(ptr::null(), entry.0) });
                if av.0.is_null() {
                    continue;
                }
                let mut parent = 0;
                let chip = if unsafe {
                    IORegistryEntryGetParentEntry(entry.0, PLANE.as_ptr(), &mut parent)
                } == 0
                {
                    let parent = OwnedIo(parent);
                    if string(property(parent.0, "EPICProviderClass", false).0).as_deref()
                        == Some("AppleDCPMCDP29XX")
                    {
                        0xb7
                    } else {
                        0x37
                    }
                } else {
                    0x37
                };
                result.push(DisplayBinding {
                    av,
                    chip,
                    identity: identity.clone(),
                });
            }
        }
        Ok(result)
    }
    fn scan(&self) -> Result<Vec<DisplayBinding>> {
        // This path uses the registry directly, without a logged-in user's
        // WindowServer display list. The installer probes it in launchd.
        let registry = self.registry_scan()?;
        if !registry.is_empty() {
            return Ok(registry);
        }
        let mut displays = [0u32; 32];
        let mut count = 0;
        let code = unsafe {
            CGGetOnlineDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count)
        };
        if code != 0 {
            return Err(format!("Display enumeration failed: CoreGraphics {code}"));
        }
        let mut results = Vec::new();
        for display in displays.into_iter().take(count as usize) {
            if unsafe { CGDisplayIsBuiltin(display) } != 0 {
                continue;
            }
            let info = OwnedCf(unsafe { (self.info)(display) });
            let Some(path) = string(dictionary_get(info.0, "IODisplayLocation")) else {
                continue;
            };
            let path = std::ffi::CString::new(path).map_err(|e| e.to_string())?;
            let framebuffer = OwnedIo(unsafe { IORegistryEntryFromPath(0, path.as_ptr()) });
            if framebuffer.0 == 0 {
                continue;
            }
            let attributes = property(framebuffer.0, "DisplayAttributes", true);
            let product = dictionary_get(attributes.0, "ProductAttributes");
            let manufacturer_id = unsafe { CGDisplayVendorNumber(display) } as u16;
            let product_id = unsafe { CGDisplayModelNumber(display) } as u16;
            let serial = string(dictionary_get(product, "AlphanumericSerialNumber"))
                .unwrap_or_else(|| unsafe { CGDisplaySerialNumber(display) }.to_string());
            let name = string(dictionary_get(product, "ProductName"))
                .unwrap_or_else(|| "External display".into());
            let manufacturer: String = [10, 5, 0]
                .iter()
                .map(|shift| (((manufacturer_id >> shift) & 31) as u8 + 64) as char)
                .collect();
            let mut selected_id = 0;
            if unsafe { IORegistryEntryGetRegistryEntryID(framebuffer.0, &mut selected_id) } != 0 {
                continue;
            }
            // Display proxies can follow their framebuffer in the registry
            // traversal without being a child of that entry. Track the exact
            // framebuffer identity, as in the published m1ddc transport.
            let root = OwnedIo(unsafe { IORegistryGetRootEntry(0) });
            let mut iterator = 0;
            if unsafe { IORegistryEntryCreateIterator(root.0, PLANE.as_ptr(), 1, &mut iterator) }
                != 0
            {
                continue;
            }
            let iterator = OwnedIo(iterator);
            let mut selected_framebuffer = false;
            loop {
                let proxy = OwnedIo(unsafe { IOIteratorNext(iterator.0) });
                if proxy.0 == 0 {
                    break;
                }
                let mut registry_id = 0;
                let mut registry_name = [0i8; 128];
                unsafe {
                    IORegistryEntryGetRegistryEntryID(proxy.0, &mut registry_id);
                    IORegistryEntryGetName(proxy.0, registry_name.as_mut_ptr());
                }
                let registry_name = unsafe { CStr::from_ptr(registry_name.as_ptr()) };
                if registry_id == selected_id
                    || unsafe { IOObjectConformsTo(proxy.0, c"IOMobileFramebuffer".as_ptr()) } != 0
                    || registry_name == c"AppleCLCD2"
                    || registry_name == c"IOMobileFramebufferShim"
                {
                    selected_framebuffer = registry_id == selected_id;
                    continue;
                }
                if !selected_framebuffer || registry_name != c"DCPAVServiceProxy" {
                    continue;
                }
                if string(property(proxy.0, "Location", true).0).as_deref() != Some("External") {
                    continue;
                }
                let av = OwnedCf(unsafe { (self.create)(ptr::null(), proxy.0) });
                if av.0.is_null() {
                    continue;
                }
                let mut parent = 0;
                let chip = if unsafe {
                    IORegistryEntryGetParentEntry(proxy.0, PLANE.as_ptr(), &mut parent)
                } == 0
                {
                    let parent = OwnedIo(parent);
                    if string(property(parent.0, "EPICProviderClass", false).0).as_deref()
                        == Some("AppleDCPMCDP29XX")
                    {
                        0xb7
                    } else {
                        0x37
                    }
                } else {
                    0x37
                };
                results.push(DisplayBinding {
                    av,
                    chip,
                    identity: MonitorIdentity {
                        manufacturer_id,
                        manufacturer: manufacturer.clone(),
                        product_id,
                        serial: serial.clone(),
                        name: name.clone(),
                        transport: format!("apple-silicon:{display}"),
                    },
                });
            }
        }
        Ok(results)
    }
}
impl Drop for DisplayApi {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.core_display);
        }
    }
}
struct DisplayBinding {
    av: OwnedCf,
    chip: u32,
    identity: MonitorIdentity,
}
pub struct MonitorBackend {
    bindings: Vec<DisplayBinding>,
    api: DisplayApi,
}
impl MonitorBackend {
    pub fn new(_: &[MonitorConfig]) -> Result<Self> {
        let api = DisplayApi::load()?;
        let bindings = api.scan()?;
        Ok(Self { bindings, api })
    }
    fn binding(&self, config: &MonitorConfig) -> Option<&DisplayBinding> {
        let mut matches = self.bindings.iter().filter(|b| b.identity.matches(config));
        let first = matches.next()?;
        if matches.next().is_some() {
            None
        } else {
            Some(first)
        }
    }
    pub fn current_input(&self, config: &MonitorConfig) -> Option<u16> {
        let binding = self.binding(config)?;
        let request = ioav_get_packet(0x60);
        if unsafe {
            (self.api.write)(
                binding.av.0,
                binding.chip,
                0x51,
                request.as_ptr().cast(),
                request.len() as u32,
            )
        } != 0
        {
            return None;
        }
        thread::sleep(Duration::from_millis(60));
        let mut reply = [0u8; 12];
        if unsafe {
            (self.api.read)(
                binding.av.0,
                binding.chip,
                0x51,
                reply.as_mut_ptr().cast(),
                reply.len() as u32,
            )
        } != 0
        {
            return None;
        }
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
                monitor: self.binding(config).unwrap().identity.clone(),
                input: input.into(),
                value,
                already_selected: true,
                verified: true,
            });
        }
        let binding = self
            .binding(config)
            .ok_or("Expected one matching reachable monitor; configure its EDID serial")?;
        let packet = set_packet(config.protocol.source(), config.protocol.vcp(), value);
        let status = unsafe {
            (self.api.write)(
                binding.av.0,
                binding.chip,
                packet[0] as u32,
                packet[1..].as_ptr().cast(),
                6,
            )
        };
        if status != 0 {
            return Err(format!("Monitor command failed: IOKit {status:#x}"));
        }
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
        Ok(backend) => MonitorStatus {
            current_input: backend.current_input(config),
            capabilities: None,
            error: None,
        },
        Err(error) => MonitorStatus {
            current_input: None,
            capabilities: None,
            error: Some(error),
        },
    }
}
