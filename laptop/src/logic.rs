//! Pure decision helpers, unit-tested without NetworkManager or Bluetooth.

use std::time::{Duration, Instant};

/// NM may sit in "connecting" forever (eduroam auth loop); after this we stop waiting for it.
pub const MAX_CONNECTING: Duration = Duration::from_secs(30);

/// Tracks how long the laptop has been without internet.
#[derive(Debug, Default)]
pub struct OfflineTimer {
    since: Option<Instant>,
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
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        let offline = now - since;
        offline >= threshold && (!connecting || offline >= MAX_CONNECTING)
    }

    pub fn offline_for(&self, now: Instant) -> Option<Duration> {
        self.since.map(|s| now - s)
    }

    pub fn reset(&mut self) {
        self.since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
