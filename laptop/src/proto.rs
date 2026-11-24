//! Classic Bluetooth (RFCOMM) protocol, keep in sync with
//! workstation-connect-android: workstation-connect/java/com/rutger/workstationconnect/Proto.kt.
//! v5: RFCOMM stream of `[len: u8][payload]` frames instead of BLE GATT; len 0 = keepalive.

use serde::Serialize;
use uuid::{Uuid, uuid};

/// RFCOMM service the phone listens on (SDP name "Workstation Connect"); channel via SDP.
pub const SERVICE: Uuid = uuid!("6f2b0010-3c1d-4e8a-9f41-7a5d2c9e0b10");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cmd {
    Off = 0,
    On = 1,
    // 2 (was "Bluetooth tethering off") retired: never reuse.
    /// Laptop's Wi-Fi network changed or dropped: drop your KDE Connect LAN links to me.
    /// Only for phones reporting version >= 2 ([`NET_CHANGED_VERSION`]).
    NetChanged = 3,
    // 4 (was "on, Bluetooth tethering only") retired: never reuse.
    /// Phone answers with a state frame.
    Ping = 5,
}

impl Cmd {
    pub fn frame(self) -> [u8; 2] {
        [1, self as u8]
    }
}

/// First phone protocol version that understands [`Cmd::NetChanged`].
pub const NET_CHANGED_VERSION: u8 = 2;

const FLAG_SHIZUKU: u8 = 1;
// 2 (was "Bluetooth tethered") unused: never reuse.
const FLAG_HOTSPOT: u8 = 4;
const FLAG_WANTED: u8 = 8;
const FLAG_ERROR: u8 = 16;

/// Splits a byte stream into frames, whatever chunks the reads return.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete frame's payload; empty for a keepalive.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let len = usize::from(*self.buf.first()?);
        if self.buf.len() < 1 + len {
            return None;
        }
        let payload = self.buf[1..1 + len].to_vec();
        self.buf.drain(..1 + len);
        Some(payload)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PhoneState {
    pub shizuku: bool,
    pub hotspot: bool,
    pub wanted: bool,
    pub error: bool,
    pub battery: u8,
    /// 0-4, None when unknown
    pub signal: Option<u8>,
    /// Phone app's protocol version.
    pub version: u8,
}

impl PhoneState {
    pub fn parse(b: &[u8]) -> Option<Self> {
        let [flags, battery, signal, version, ..] = *b else {
            return None;
        };
        Some(Self {
            shizuku: flags & FLAG_SHIZUKU != 0,
            hotspot: flags & FLAG_HOTSPOT != 0,
            wanted: flags & FLAG_WANTED != 0,
            error: flags & FLAG_ERROR != 0,
            battery,
            signal: (signal <= 4).then_some(signal),
            version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state() {
        let s = PhoneState::parse(&[1 | 4 | 8, 87, 3, 5]).unwrap();
        assert!(s.shizuku && s.hotspot && s.wanted && !s.error);
        // Old phones may still set the retired BT flag: ignored.
        let old = PhoneState::parse(&[1 | 2, 50, 2, 3]).unwrap();
        assert!(old.shizuku && !old.hotspot && !old.wanted && !old.error);
        assert_eq!((s.battery, s.signal, s.version), (87, Some(3), 5));
        assert_eq!(PhoneState::parse(&[0, 50, 255, 1]).unwrap().signal, None);
        assert!(PhoneState::parse(&[0, 1]).is_none());
        // Extra bytes from newer phones: ignored.
        assert_eq!(PhoneState::parse(&[1 | 4 | 8, 87, 3, 5, 9, 9]), Some(s));
    }

    #[test]
    fn encodes_commands() {
        assert_eq!(Cmd::Off.frame(), [1, 0]);
        assert_eq!(Cmd::On.frame(), [1, 1]);
        assert_eq!(Cmd::NetChanged.frame(), [1, 3]);
        assert_eq!(Cmd::Ping.frame(), [1, 5]);
    }

    #[test]
    fn decodes_partial_reads() {
        let mut d = Decoder::default();
        d.feed(&[4, 1, 87]);
        assert_eq!(d.next_frame(), None);
        d.feed(&[3]);
        assert_eq!(d.next_frame(), None);
        d.feed(&[5]);
        assert_eq!(d.next_frame(), Some(vec![1, 87, 3, 5]));
        assert_eq!(d.next_frame(), None);
        // Even the length byte alone.
        d.feed(&[]);
        assert_eq!(d.next_frame(), None);
    }

    #[test]
    fn decodes_multiple_frames_and_keepalives() {
        let mut d = Decoder::default();
        d.feed(&[0, 4, 1, 87, 3, 5, 0, 5, 4, 50, 255, 5, 7, 1]);
        assert_eq!(d.next_frame(), Some(vec![]));
        assert_eq!(d.next_frame(), Some(vec![1, 87, 3, 5]));
        assert_eq!(d.next_frame(), Some(vec![]));
        assert_eq!(d.next_frame(), Some(vec![4, 50, 255, 5, 7]));
        assert_eq!(d.next_frame(), None);
        d.feed(&[2]);
        assert_eq!(d.next_frame(), Some(vec![2]));
        assert_eq!(d.next_frame(), None);
    }
}
