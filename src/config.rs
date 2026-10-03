use crate::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub usb: UsbSelector,
    pub local_input: String,
    pub remote_input: String,
    pub monitors: Vec<MonitorConfig>,
    #[serde(default = "default_debounce")]
    pub debounce_ms: u64,
    #[serde(default = "default_resume_guard")]
    pub resume_guard_ms: u64,
    #[serde(default = "default_attempts")]
    pub arrival_attempts: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
    #[serde(default)]
    pub reconcile_on_start: bool,
}
fn default_debounce() -> u64 {
    200
}
fn default_resume_guard() -> u64 {
    1500
}
fn default_attempts() -> u32 {
    3
}
fn default_retry_delay() -> u64 {
    250
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsbSelector {
    pub vendor_id: u16,
    pub product_id: u16,
    #[serde(default)]
    pub serial: Option<String>,
    #[serde(default)]
    pub instance_contains: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct UsbDevice {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: String,
    pub instance: String,
}

impl UsbSelector {
    pub fn matches(&self, device: &UsbDevice) -> bool {
        self.vendor_id == device.vendor_id
            && self.product_id == device.product_id
            && self
                .serial
                .as_ref()
                .is_none_or(|s| s.eq_ignore_ascii_case(&device.serial))
            && self.instance_contains.as_ref().is_none_or(|s| {
                device
                    .instance
                    .to_ascii_lowercase()
                    .contains(&s.to_ascii_lowercase())
            })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    pub manufacturer_id: u16,
    pub product_id: u16,
    #[serde(default)]
    pub serial: Option<String>,
    pub protocol: Protocol,
    pub inputs: BTreeMap<String, u16>,
    #[serde(default)]
    pub readback: BTreeMap<String, u16>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Standard,
    LgAlternate,
}
impl Protocol {
    pub fn source(self) -> u8 {
        match self {
            Self::Standard => 0x51,
            Self::LgAlternate => 0x50,
        }
    }
    pub fn vcp(self) -> u8 {
        match self {
            Self::Standard => 0x60,
            Self::LgAlternate => 0xf4,
        }
    }
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let config: Self =
            serde_json::from_str(text).map_err(|e| format!("Invalid configuration: {e}"))?;
        Ok(config)
    }
    pub fn load(path: &Path) -> Result<Self> {
        let config = Self::read(path)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err("Only schema_version 1 is supported".into());
        }
        if self.monitors.is_empty() {
            return Err("Configure at least one monitor".into());
        }
        if self.local_input == self.remote_input {
            return Err("Local and remote inputs must differ".into());
        }
        if !(20..=5000).contains(&self.debounce_ms) {
            return Err("debounce_ms must be 20..5000".into());
        }
        if !(100..=10000).contains(&self.resume_guard_ms) {
            return Err("resume_guard_ms must be 100..10000".into());
        }
        if !(1..=5).contains(&self.arrival_attempts) {
            return Err("arrival_attempts must be 1..5".into());
        }
        if !(20..=5000).contains(&self.retry_delay_ms) {
            return Err("retry_delay_ms must be 20..5000".into());
        }
        if self
            .usb
            .serial
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
            || self
                .usb
                .instance_contains
                .as_ref()
                .is_some_and(|s| s.trim().is_empty())
        {
            return Err("USB serial/instance filters cannot be empty strings".into());
        }
        if self
            .usb
            .instance_contains
            .as_ref()
            .is_some_and(|s| s.starts_with("REPLACE_WITH_"))
        {
            return Err("Switch hub has not been configured. Run learn-switch --config PATH --output PATH, or supply the hub's exact identity.".into());
        }
        for (index, monitor) in self.monitors.iter().enumerate() {
            for input in [&self.local_input, &self.remote_input] {
                if !monitor.inputs.contains_key(input) {
                    return Err(format!("Monitor {index} has no input named {input}"));
                }
            }
            if monitor.serial.as_ref().is_some_and(|s| s.trim().is_empty()) {
                return Err(format!("Monitor {index} has an empty serial"));
            }
        }
        Ok(())
    }
}

/// Device-interface paths are supplied by Windows even after removal, when
/// querying properties of the removed node would no longer be reliable.
pub fn usb_from_windows_path(path: &str) -> Option<UsbDevice> {
    let parts: Vec<_> = path.trim_start_matches("\\\\?\\").split('#').collect();
    if parts.len() < 3 || !parts[0].eq_ignore_ascii_case("usb") {
        return None;
    }
    let ids = parts[1].to_ascii_uppercase();
    let vendor = ids.find("VID_")? + 4;
    let product = ids.find("PID_")? + 4;
    Some(UsbDevice {
        vendor_id: u16::from_str_radix(ids.get(vendor..vendor + 4)?, 16).ok()?,
        product_id: u16::from_str_radix(ids.get(product..product + 4)?, 16).ok()?,
        serial: parts[2].into(),
        instance: format!("USB\\{}\\{}", parts[1], parts[2]),
    })
}
