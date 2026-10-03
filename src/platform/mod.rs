use crate::{
    Result,
    config::{MonitorConfig, UsbSelector},
    ddc::{MonitorIdentity, SwitchOutcome},
};
use std::sync::{Arc, atomic::AtomicU64, mpsc::SyncSender};

#[derive(Clone, Debug)]
pub enum Event {
    UsbChanged,
    Suspend,
    Resume,
    Stop,
}

#[derive(Clone)]
pub struct EventSink {
    pub sender: SyncSender<Event>,
    pub generation: Arc<AtomicU64>,
    pub selector: Option<UsbSelector>,
}
impl EventSink {
    pub fn emit(&self, event: Event) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = self.sender.try_send(event);
    }
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub mod windows_service;
#[cfg(windows)]
pub use windows::{
    InstanceLock, MonitorBackend, Watcher, detach_console, full_monitor_status, monitors,
    usb_devices,
};
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    InstanceLock, MonitorBackend, Watcher, detach_console, full_monitor_status, monitors,
    usb_devices,
};

#[derive(Default, serde::Serialize)]
pub struct MonitorStatus {
    pub current_input: Option<u16>,
    pub capabilities: Option<String>,
    pub error: Option<String>,
}
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    InstanceLock, MonitorBackend, Watcher, detach_console, full_monitor_status, monitors,
    usb_devices,
};
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
compile_error!("digital-kvm supports Windows, macOS, and Linux");

pub trait MonitorControl {
    fn identities(&self) -> Vec<MonitorIdentity>;
    fn set_input(&mut self, config: &MonitorConfig, input: &str) -> Result<SwitchOutcome>;
}
