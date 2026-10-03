use crate::{Result, config::MonitorConfig};
use serde::Serialize;

pub fn set_packet(source: u8, vcp: u8, value: u16) -> [u8; 7] {
    let mut bytes = [source, 0x84, 0x03, vcp, (value >> 8) as u8, value as u8, 0];
    bytes[6] = bytes[..6]
        .iter()
        .fold(0x6e, |checksum, byte| checksum ^ byte);
    bytes
}

/// IOAVService's Get VCP path excludes its data-address argument from the
/// request checksum. Its Set VCP path uses the ordinary packet above.
pub fn ioav_get_packet(vcp: u8) -> [u8; 4] {
    [0x82, 0x01, vcp, 0x6e ^ 0x82 ^ 0x01 ^ vcp]
}

pub fn parse_vcp_reply(reply: &[u8], vcp: u8) -> Option<u16> {
    if reply.len() < 11
        || reply[0] != 0x6e
        || reply[1] != 0x88
        || reply[2] != 2
        || reply[3] != 0
        || reply[4] != vcp
        || reply[..11].iter().fold(0x50u8, |a, b| a ^ b) != 0
    {
        return None;
    }
    Some(u16::from_be_bytes([reply[8], reply[9]]))
}

#[derive(Clone, Debug, Serialize)]
pub struct MonitorIdentity {
    pub manufacturer_id: u16,
    pub manufacturer: String,
    pub product_id: u16,
    pub serial: String,
    pub name: String,
    pub transport: String,
}
impl MonitorIdentity {
    pub fn matches(&self, config: &MonitorConfig) -> bool {
        self.manufacturer_id == config.manufacturer_id
            && self.product_id == config.product_id
            && config
                .serial
                .as_ref()
                .is_none_or(|s| s.eq_ignore_ascii_case(&self.serial))
    }
}

pub fn parse_edid(bytes: &[u8], transport: String) -> Result<MonitorIdentity> {
    if bytes.len() < 128 || bytes[..8] != [0, 255, 255, 255, 255, 255, 255, 0] {
        return Err("Invalid EDID header".into());
    }
    if bytes[..128]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        != 0
    {
        return Err("Invalid EDID checksum".into());
    }
    let vendor = u16::from_be_bytes([bytes[8], bytes[9]]);
    let manufacturer = [10, 5, 0]
        .iter()
        .map(|shift| (((vendor >> shift) & 31) as u8 + 64) as char)
        .collect();
    let mut serial = String::new();
    let mut name = String::new();
    for descriptor in bytes[54..126].as_chunks::<18>().0 {
        if descriptor[..3] == [0, 0, 0] {
            let text = String::from_utf8_lossy(&descriptor[5..18])
                .trim_matches(['\0', '\n', '\r', ' '])
                .to_string();
            match descriptor[3] {
                0xfc => name = text,
                0xff => serial = text,
                _ => {}
            }
        }
    }
    if serial.is_empty() {
        serial = u32::from_le_bytes(bytes[12..16].try_into().unwrap()).to_string();
    }
    Ok(MonitorIdentity {
        manufacturer_id: vendor,
        manufacturer,
        product_id: u16::from_le_bytes([bytes[10], bytes[11]]),
        serial,
        name,
        transport,
    })
}

#[derive(Debug, Serialize)]
pub struct SwitchOutcome {
    pub monitor: MonitorIdentity,
    pub input: String,
    pub value: u16,
    pub already_selected: bool,
    /// Transport success is distinct from observing the actual monitor input.
    pub verified: bool,
}
