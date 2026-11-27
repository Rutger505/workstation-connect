//! KDE Connect over Tailscale: without a shared LAN (eduroam client isolation, school Wi-Fi with
//! the hotspot off) KDE Connect's UDP broadcast never reaches the phone. Both sides always run
//! Tailscale, so each puts the other's Tailscale IPv4 in its KDE Connect custom devices; the
//! addresses travel over the Bluetooth link (protocol v6).

use anyhow::{Context, Result};
use std::ffi::CStr;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use tracing::debug;

pub const IFACE: &str = "tailscale0";

/// Tailscale's CGNAT range, 100.64.0.0/10.
pub fn is_tailscale(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// The laptop's own Tailscale IPv4: first 100.64.0.0/10 address on `tailscale0`.
pub fn own_ipv4() -> std::io::Result<Option<Ipv4Addr>> {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills ifap with a list we free below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut found = None;
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: cur is a node of the list getifaddrs returned, valid until freeifaddrs.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
            continue;
        }
        // SAFETY: non-null, NUL-terminated name from getifaddrs.
        if unsafe { CStr::from_ptr(ifa.ifa_name) }.to_bytes() != IFACE.as_bytes() {
            continue;
        }
        // SAFETY: non-null sockaddr; only read as sockaddr_in once the family says AF_INET.
        if i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
        if is_tailscale(ip) {
            found = Some(ip);
            break;
        }
    }
    // SAFETY: ifap came from a successful getifaddrs and isn't used afterwards.
    unsafe { libc::freeifaddrs(ifap) };
    Ok(found)
}

/// The custom device entry the daemon manages (the phone's Tailscale IP it last added): replaced
/// when the phone's IP changes, entries Rutger added himself stay.
fn managed_path() -> PathBuf {
    crate::config::state_dir().join("kdeconnect-custom-device")
}

pub fn load_managed() -> Option<Ipv4Addr> {
    let path = managed_path();
    match std::fs::read_to_string(&path) {
        Ok(t) => match t.trim().parse() {
            Ok(ip) => Some(ip),
            Err(e) => {
                tracing::warn!(
                    "{}: {:?} unreadable ({e}), ignoring",
                    path.display(),
                    t.trim()
                );
                None
            }
        },
        Err(e) => {
            debug!("{}: {e}: no managed custom device yet", path.display());
            None
        }
    }
}

pub fn save_managed(ip: Ipv4Addr) -> Result<()> {
    let path = managed_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| dir.display().to_string())?;
    }
    std::fs::write(&path, format!("{ip}\n")).with_context(|| path.display().to_string())
}

/// `current` with `new` in it and the previously managed entry gone (if it differs); order kept.
pub fn merge(current: &[String], managed: Option<Ipv4Addr>, new: Ipv4Addr) -> Vec<String> {
    let old = managed.filter(|m| *m != new).map(|m| m.to_string());
    let new = new.to_string();
    let mut out: Vec<String> = current
        .iter()
        .filter(|e| Some(e.trim()) != old.as_deref())
        .cloned()
        .collect();
    if !out.iter().any(|e| e.trim() == new) {
        out.push(new);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_tailscale(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailscale(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_tailscale(Ipv4Addr::new(100, 63, 0, 1)));
        assert!(!is_tailscale(Ipv4Addr::new(192, 168, 1, 2)));
    }

    #[test]
    fn merges_custom_devices() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Empty list: added.
        assert_eq!(merge(&[], None, ip("100.64.1.2")), l(&["100.64.1.2"]));
        // Rutger's own entries stay, the old managed one is replaced.
        assert_eq!(
            merge(
                &l(&["192.168.1.5", "100.64.1.2"]),
                Some(ip("100.64.1.2")),
                ip("100.64.9.9")
            ),
            l(&["192.168.1.5", "100.64.9.9"])
        );
        // Already there: unchanged.
        assert_eq!(
            merge(
                &l(&["100.64.1.2", "host.lan"]),
                Some(ip("100.64.1.2")),
                ip("100.64.1.2")
            ),
            l(&["100.64.1.2", "host.lan"])
        );
        // Old managed entry removed by hand meanwhile: just add the new one.
        assert_eq!(
            merge(&l(&["host.lan"]), Some(ip("100.64.1.2")), ip("100.64.9.9")),
            l(&["host.lan", "100.64.9.9"])
        );
    }

    #[test]
    fn own_ipv4_does_not_fail() {
        // Whatever the machine has: only checks the getifaddrs walk itself.
        let ip = own_ipv4().unwrap();
        assert!(ip.is_none_or(is_tailscale));
    }
}
