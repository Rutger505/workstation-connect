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

/// kdeconnectd watchdog: probe interval, and the earliest re-probe when a KDE Connect call timed out.
pub const KDE_PROBE_EVERY: Duration = Duration::from_secs(120);
pub const KDE_PROBE_MIN_GAP: Duration = Duration::from_secs(30);
/// Consecutive hung probes before a restart.
pub const KDE_HUNG_LIMIT: u32 = 2;
/// At most one restart per this window: no restart loop when a restart doesn't help.
pub const KDE_RESTART_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// One kdeconnectd health probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KdeProbe {
    Ok,
    /// Not on the bus: D-Bus activation starts it when needed, nothing to fix.
    NotRunning,
    /// On the bus, but no answer in time.
    Hung,
    /// Probe itself failed (bus trouble, error reply): says nothing about a hang.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KdeVerdict {
    Fine,
    /// No new information (probe error): counter kept.
    Unchanged,
    /// Hung, `n` in a row, below the limit.
    Hung(u32),
    /// Restart now (recorded as done: a failed restart also counts for the backoff).
    Restart,
    /// Hung past the limit, but restarted less than the backoff ago. `first`: first skip of
    /// this hang (log it once).
    Backoff {
        left: Duration,
        first: bool,
    },
}

#[derive(Debug, Default)]
pub struct KdeWatchdog {
    hung: u32,
    last_restart: Option<Instant>,
    backoff_noted: bool,
}

impl KdeWatchdog {
    pub fn observe(&mut self, probe: KdeProbe, now: Instant) -> KdeVerdict {
        match probe {
            KdeProbe::Ok | KdeProbe::NotRunning => {
                self.hung = 0;
                self.backoff_noted = false;
                KdeVerdict::Fine
            }
            KdeProbe::Error => KdeVerdict::Unchanged,
            KdeProbe::Hung => {
                self.hung += 1;
                if self.hung < KDE_HUNG_LIMIT {
                    return KdeVerdict::Hung(self.hung);
                }
                if let Some(t) = self.last_restart
                    && now - t < KDE_RESTART_BACKOFF
                {
                    let first = !self.backoff_noted;
                    self.backoff_noted = true;
                    return KdeVerdict::Backoff {
                        left: KDE_RESTART_BACKOFF - (now - t),
                        first,
                    };
                }
                self.hung = 0;
                self.backoff_noted = false;
                self.last_restart = Some(now);
                KdeVerdict::Restart
            }
        }
    }

    /// Probe now (a KDE Connect call timed out) instead of waiting for the interval?
    pub fn early_probe_ok(last_probe: Instant, now: Instant) -> bool {
        now - last_probe >= KDE_PROBE_MIN_GAP
    }
}

/// Adapter back: wait this long before restarting kdeconnectd (MT7921 needs ~2.5 s of setup
/// after btusb loads; Powered can flap meanwhile).
pub const BT_ADAPTER_SETTLE: Duration = Duration::from_secs(3);
/// At most one adapter-triggered kdeconnectd restart per this gap (a flapping adapter).
pub const BT_ADAPTER_RESTART_GAP: Duration = Duration::from_secs(30);

/// Bluetooth adapter usable = present and powered. kdeconnectd's Bluetooth backend looks for an
/// adapter only at start and resume ("No local bluetooth adapter found") and never again, so
/// after a gap (btusb reloaded on suspend, rfkill, bluetoothd restart) it needs a restart.
#[derive(Debug, Default)]
pub struct BtAdapterWatch {
    usable: Option<bool>,
}

impl BtAdapterWatch {
    /// Returns true when the adapter just came back after being seen gone. The first
    /// observation never counts: kdeconnectd's own start covers that state.
    pub fn observe(&mut self, usable: bool) -> bool {
        let back = self.usable == Some(false) && usable;
        self.usable = Some(usable);
        back
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt_adapter_back_only_after_a_gap() {
        let mut w = BtAdapterWatch::default();
        assert!(!w.observe(true), "startup state never counts");
        assert!(!w.observe(true));
        assert!(!w.observe(false));
        assert!(!w.observe(false));
        assert!(w.observe(true), "back after gone");
        assert!(!w.observe(true), "once per gap");
    }

    #[test]
    fn bt_adapter_absent_at_start_then_back() {
        let mut w = BtAdapterWatch::default();
        assert!(!w.observe(false));
        assert!(w.observe(true));
    }

    #[test]
    fn kde_watchdog_restarts_after_two_hangs() {
        let t0 = Instant::now();
        let mut w = KdeWatchdog::default();
        assert_eq!(w.observe(KdeProbe::Hung, t0), KdeVerdict::Hung(1));
        // Any answer in between resets the count.
        assert_eq!(w.observe(KdeProbe::Ok, t0), KdeVerdict::Fine);
        assert_eq!(w.observe(KdeProbe::Hung, t0), KdeVerdict::Hung(1));
        assert_eq!(w.observe(KdeProbe::NotRunning, t0), KdeVerdict::Fine);
        assert_eq!(w.observe(KdeProbe::Hung, t0), KdeVerdict::Hung(1));
        // Probe errors neither count nor reset.
        assert_eq!(w.observe(KdeProbe::Error, t0), KdeVerdict::Unchanged);
        assert_eq!(w.observe(KdeProbe::Hung, t0), KdeVerdict::Restart);
        // Counting starts over after a restart.
        assert_eq!(w.observe(KdeProbe::Hung, t0), KdeVerdict::Hung(1));
    }

    #[test]
    fn kde_watchdog_backs_off_between_restarts() {
        let t0 = Instant::now();
        let min = |m: u64| t0 + Duration::from_secs(m * 60);
        let mut w = KdeWatchdog::default();
        w.observe(KdeProbe::Hung, t0);
        assert_eq!(w.observe(KdeProbe::Hung, min(2)), KdeVerdict::Restart);
        // Still hung after the restart: skipped until 10 min after it, logged once.
        assert_eq!(w.observe(KdeProbe::Hung, min(4)), KdeVerdict::Hung(1));
        assert_eq!(
            w.observe(KdeProbe::Hung, min(6)),
            KdeVerdict::Backoff {
                left: Duration::from_secs(6 * 60),
                first: true
            }
        );
        assert_eq!(
            w.observe(KdeProbe::Hung, min(8)),
            KdeVerdict::Backoff {
                left: Duration::from_secs(4 * 60),
                first: false
            }
        );
        assert_eq!(w.observe(KdeProbe::Hung, min(12)), KdeVerdict::Restart);
        // A recovery in between makes the next backoff skip log again.
        w.observe(KdeProbe::Hung, min(13));
        assert!(matches!(
            w.observe(KdeProbe::Hung, min(14)),
            KdeVerdict::Backoff { first: true, .. }
        ));
        w.observe(KdeProbe::Ok, min(15));
        w.observe(KdeProbe::Hung, min(16));
        assert!(matches!(
            w.observe(KdeProbe::Hung, min(17)),
            KdeVerdict::Backoff { first: true, .. }
        ));
    }

    #[test]
    fn kde_early_probe_gap() {
        let t0 = Instant::now();
        assert!(!KdeWatchdog::early_probe_ok(
            t0,
            t0 + Duration::from_secs(29)
        ));
        assert!(KdeWatchdog::early_probe_ok(t0, t0 + KDE_PROBE_MIN_GAP));
    }

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
