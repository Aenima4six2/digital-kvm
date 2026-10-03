//! Native Service Control Manager entry point and lifecycle.
use crate::{Result, platform::Event};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
static MAIN: OnceLock<fn() -> Result<()>> = OnceLock::new();
static NAME: OnceLock<Vec<u16>> = OnceLock::new();
static HANDLE: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
#[repr(C)]
struct Entry {
    name: *mut u16,
    main: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
}
#[repr(C)]
struct Status {
    kind: u32,
    state: u32,
    accepted: u32,
    win32_exit: u32,
    specific_exit: u32,
    checkpoint: u32,
    wait_hint: u32,
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn StartServiceCtrlDispatcherW(entries: *const Entry) -> i32;
    fn RegisterServiceCtrlHandlerExW(
        name: *const u16,
        callback: unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32,
        context: *mut c_void,
    ) -> *mut c_void;
    fn SetServiceStatus(handle: *mut c_void, status: *const Status) -> i32;
}
fn report(state: u32, failed: bool) {
    let handle = HANDLE.load(Ordering::SeqCst) as *mut c_void;
    if handle.is_null() {
        return;
    }
    let status = Status {
        kind: 0x10,
        state,
        accepted: if state == 4 { 1 | 4 | 64 } else { 0 },
        win32_exit: if failed { 1066 } else { 0 },
        specific_exit: if failed { 1 } else { 0 },
        checkpoint: if state == 2 || state == 3 { 1 } else { 0 },
        wait_hint: if state == 2 || state == 3 { 15000 } else { 0 },
    };
    unsafe {
        SetServiceStatus(handle, &status);
    }
}
unsafe extern "system" fn control(kind: u32, power: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match kind {
        1 | 5 => {
            STOP.store(true, Ordering::SeqCst);
            report(3, false);
            super::windows::emit_service_event(Event::Stop);
        }
        13 => match power {
            4 => super::windows::emit_service_event(Event::Suspend),
            7 | 18 => super::windows::emit_service_event(Event::Resume),
            _ => {}
        },
        4 => report(if STOP.load(Ordering::SeqCst) { 3 } else { 4 }, false),
        _ => {}
    }
    0
}
unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(NAME.get().unwrap().as_ptr(), control, ptr::null_mut())
    };
    if handle.is_null() {
        return;
    }
    HANDLE.store(handle as usize, Ordering::SeqCst);
    report(2, false);
    report(4, false);
    let result = MAIN.get().unwrap()();
    report(1, result.is_err());
}
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}
pub fn dispatch(name: &str, main: fn() -> Result<()>) -> Result<()> {
    MAIN.set(main)
        .map_err(|_| "Service dispatcher was already initialized")?;
    NAME.set(name.encode_utf16().chain(Some(0)).collect())
        .map_err(|_| "Service name was already initialized")?;
    let entries = [
        Entry {
            name: NAME.get().unwrap().as_ptr() as *mut u16,
            main: Some(service_main),
        },
        Entry {
            name: ptr::null_mut(),
            main: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(entries.as_ptr()) } == 0 {
        return Err(format!(
            "Cannot connect to Windows Service Control Manager: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}
