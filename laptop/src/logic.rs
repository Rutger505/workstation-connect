//! Pure decision helpers, unit-tested without NetworkManager or Bluetooth.

use std::time::{Duration, Instant};

/// NM may sit in "connecting" forever (eduroam auth loop); after this we stop waiting for it.
pub const MAX_CONNECTING: Duration = Duration::from_secs(30);

/// Between Wi-Fi scan attempts while the first scan of an offline spell keeps failing.
pub const SCAN_RETRY: Duration = Duration::from_secs(3);

/// Tracks how long the laptop has been without internet, and whether a saved Wi-Fi network is in
/// range for NetworkManager to join. None in range: no point waiting, ask the phone right away.
#[derive(Debug, Default)]
pub struct OfflineTimer {
    since: Option<Instant>,
    /// Scan result for this offline spell; None = not scanned (yet).
    known_in_range: Option<bool>,
    last_scan: Option<Instant>,
}

impl OfflineTimer {
    /// Returns true when the phone should be asked for internet.
    pub fn update(
        &mut self,
        online: bool,
        connecting: bool,
        now: Instant,
        threshold: Duration,
    ) -> bool {
        if online {
            self.reset();
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if self.known_in_range == Some(false) {
            return true;
        }
        // Known network in range (or scan failed): give NM its chance first.
        let offline = now - since;
        offline >= threshold && (!connecting || offline >= MAX_CONNECTING)
    }

    /// Offline and no scan result yet for this spell (rate-limited while scans fail).
    pub fn needs_scan(&self, now: Instant) -> bool {
        self.since.is_some()
            && self.known_in_range.is_none()
            && self.last_scan.is_none_or(|t| now - t >= SCAN_RETRY)
    }

    /// `Some(known in range)`, or None when the scan failed.
    pub fn scanned(&mut self, known_in_range: Option<bool>, now: Instant) {
        self.last_scan = Some(now);
        if self.since.is_some() {
            self.known_in_range = known_in_range;
        }
    }

    pub fn offline_for(&self, now: Instant) -> Option<Duration> {
        self.since.map(|s| now - s)
    }

    /// New spell: also after resume, where the old scan is stale.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The Wi-Fi device's active connection (NM object path, "/" = none) moved away from one it had:
/// the phone's KDE Connect LAN links to us are dead. Connecting from nothing isn't a change.
pub fn wifi_left(prev: &str, new: &str) -> bool {
    prev != "/" && prev != new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_left_only_when_leaving_a_connection() {
        let (a, b) = (
            "/org/freedesktop/NetworkManager/ActiveConnection/1",
            "/org/freedesktop/NetworkManager/ActiveConnection/2",
        );
        assert!(wifi_left(a, "/"));
        assert!(wifi_left(a, b));
        assert!(!wifi_left("/", a));
        assert!(!wifi_left(a, a));
        assert!(!wifi_left("/", "/"));
    }

    const T: Duration = Duration::from_secs(10);

    #[test]
    fn triggers_after_threshold() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        assert!(!o.update(false, false, t0, T));
        assert!(!o.update(false, false, t0 + Duration::from_secs(9), T));
        assert!(o.update(false, false, t0 + Duration::from_secs(10), T));
    }

    #[test]
    fn online_resets() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        o.update(false, false, t0, T);
        o.update(true, false, t0 + Duration::from_secs(8), T);
        assert!(!o.update(false, false, t0 + Duration::from_secs(12), T));
        assert!(o.update(false, false, t0 + Duration::from_secs(22), T));
    }

    #[test]
    fn waits_for_connecting_but_not_forever() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        o.update(false, true, t0, T);
        assert!(!o.update(false, true, t0 + Duration::from_secs(15), T));
        assert!(o.update(false, false, t0 + Duration::from_secs(16), T));
        let mut o = OfflineTimer::default();
        o.update(false, true, t0, T);
        assert!(o.update(false, true, t0 + MAX_CONNECTING, T));
    }

    #[test]
    fn no_known_wifi_triggers_at_once() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        assert!(!o.needs_scan(t0));
        assert!(!o.update(false, true, t0, T));
        assert!(o.needs_scan(t0));
        o.scanned(Some(false), t0 + Duration::from_secs(2));
        assert!(!o.needs_scan(t0 + Duration::from_secs(9)));
        assert!(o.update(false, true, t0 + Duration::from_secs(2), T));
    }

    #[test]
    fn known_wifi_waits_threshold() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        o.update(false, false, t0, T);
        o.scanned(Some(true), t0);
        assert!(!o.update(false, false, t0 + Duration::from_secs(9), T));
        assert!(o.update(false, false, t0 + T, T));
    }

    #[test]
    fn failed_scan_retries_and_falls_back_to_threshold() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        o.update(false, false, t0, T);
        o.scanned(None, t0);
        assert!(!o.needs_scan(t0 + Duration::from_secs(1)));
        assert!(o.needs_scan(t0 + SCAN_RETRY));
        assert!(!o.update(false, false, t0 + Duration::from_secs(9), T));
        assert!(o.update(false, false, t0 + T, T));
    }

    #[test]
    fn online_and_reset_forget_the_scan() {
        let t0 = Instant::now();
        let mut o = OfflineTimer::default();
        o.update(false, false, t0, T);
        o.scanned(Some(false), t0);
        o.update(true, false, t0, T);
        assert!(!o.update(false, false, t0, T));
        o.scanned(Some(false), t0);
        o.reset();
        assert!(!o.update(false, false, t0, T));
        assert!(o.needs_scan(t0));
    }
}
