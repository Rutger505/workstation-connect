//! BLE protocol, keep in sync with android/app/src/main/java/com/rutger/wsconnect/Proto.kt.

use serde::Serialize;
use uuid::{Uuid, uuid};

pub const SERVICE: Uuid = uuid!("6f2b0001-3c1d-4e8a-9f41-7a5d2c9e0b10");
pub const CONTROL: Uuid = uuid!("6f2b0002-3c1d-4e8a-9f41-7a5d2c9e0b10");
pub const STATE: Uuid = uuid!("6f2b0003-3c1d-4e8a-9f41-7a5d2c9e0b10");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cmd {
    Off = 0,
    On = 1,
    BtOff = 2,
}

const FLAG_SHIZUKU: u8 = 1;
const FLAG_BT_TETHER: u8 = 2;
const FLAG_HOTSPOT: u8 = 4;
const FLAG_WANTED: u8 = 8;
const FLAG_ERROR: u8 = 16;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PhoneState {
    pub shizuku: bool,
    pub bt_tether: bool,
    pub hotspot: bool,
    pub wanted: bool,
    pub error: bool,
    pub battery: u8,
    /// 0-4, None when unknown
    pub signal: Option<u8>,
}

impl PhoneState {
    pub fn parse(b: &[u8]) -> Option<Self> {
        let [flags, battery, signal, _version, ..] = *b else {
            return None;
        };
        Some(Self {
            shizuku: flags & FLAG_SHIZUKU != 0,
            bt_tether: flags & FLAG_BT_TETHER != 0,
            hotspot: flags & FLAG_HOTSPOT != 0,
            wanted: flags & FLAG_WANTED != 0,
            error: flags & FLAG_ERROR != 0,
            battery,
            signal: (signal <= 4).then_some(signal),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state() {
        let s = PhoneState::parse(&[1 | 4 | 8, 87, 3, 1]).unwrap();
        assert!(s.shizuku && s.hotspot && s.wanted && !s.bt_tether && !s.error);
        assert_eq!((s.battery, s.signal), (87, Some(3)));
        assert_eq!(PhoneState::parse(&[0, 50, 255, 1]).unwrap().signal, None);
        assert!(PhoneState::parse(&[0, 1]).is_none());
    }
}
