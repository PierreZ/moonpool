//! UTC validation time: a separate input from the runtime's scheduling
//! clock.
//!
//! [`TimeProvider::now`](moonpool_core::TimeProvider::now) is logical
//! scheduling time (simulated in a simulation, monotonic in production),
//! never Unix time, so it cannot tell whether a certificate or a token is
//! valid. Credential and certificate validation read a [`UtcClock`]
//! instead, supplied explicitly: [`SystemUtc`] in production, a scripted
//! [`FixedUtc`] in simulation and tests. Nothing in this crate reads the
//! host clock unless it is handed a [`SystemUtc`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A point in UTC, in whole seconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTime(u64);

impl UtcTime {
    /// The instant `seconds` after the Unix epoch.
    #[must_use]
    pub const fn from_unix_seconds(seconds: u64) -> Self {
        Self(seconds)
    }

    /// Seconds since the Unix epoch.
    #[must_use]
    pub const fn unix_seconds(self) -> u64 {
        self.0
    }

    /// `self` moved `seconds` later, saturating.
    #[must_use]
    pub const fn plus(self, seconds: u64) -> Self {
        Self(self.0.saturating_add(seconds))
    }

    /// `self` moved `seconds` earlier, saturating at the epoch.
    #[must_use]
    pub const fn minus(self, seconds: u64) -> Self {
        Self(self.0.saturating_sub(seconds))
    }
}

/// Where credential and certificate validation read the current UTC time.
///
/// Not the runtime's [`TimeProvider`](moonpool_core::TimeProvider): that is
/// scheduling time. `None` means the time is unknown, and every check that
/// needs it fails closed.
pub trait UtcClock: Send + Sync + 'static {
    /// The current UTC time, or `None` when it is unknown.
    fn now_utc(&self) -> Option<UtcTime>;
}

/// A clock that never knows the time: every time-dependent check fails
/// closed. The default of [`SecurityConfig`](crate::security::SecurityConfig),
/// so a deployment that verifies expiring credentials must choose a clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoUtc;

impl UtcClock for NoUtc {
    fn now_utc(&self) -> Option<UtcTime> {
        None
    }
}

/// A scripted UTC clock: the time a test or a simulation sets, and nothing
/// else. Clones share the same time.
#[derive(Debug, Clone)]
pub struct FixedUtc {
    // `u64::MAX` stands for "unknown".
    seconds: Arc<AtomicU64>,
}

impl FixedUtc {
    /// A clock showing `now`.
    #[must_use]
    pub fn new(now: UtcTime) -> Self {
        Self {
            seconds: Arc::new(AtomicU64::new(now.0.min(u64::MAX - 1))),
        }
    }

    /// Show `now` from here on (to every clone).
    pub fn set(&self, now: UtcTime) {
        self.seconds
            .store(now.0.min(u64::MAX - 1), Ordering::Relaxed);
    }

    /// Move the shown time `seconds` later.
    pub fn advance(&self, seconds: u64) {
        let _ = self
            .seconds
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
                (now != u64::MAX).then(|| now.saturating_add(seconds).min(u64::MAX - 1))
            });
    }

    /// Make the time unknown (a clock that lost its source).
    pub fn forget(&self) {
        self.seconds.store(u64::MAX, Ordering::Relaxed);
    }
}

impl UtcClock for FixedUtc {
    fn now_utc(&self) -> Option<UtcTime> {
        let seconds = self.seconds.load(Ordering::Relaxed);
        (seconds != u64::MAX).then_some(UtcTime(seconds))
    }
}

/// The host's real-time clock (`std::time::SystemTime`), for production.
///
/// Never use it inside a simulation: it is host time, different on every
/// run. Not available on `wasm32-unknown-unknown`, where the standard
/// library has no clock.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemUtc;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
impl UtcClock for SystemUtc {
    fn now_utc(&self) -> Option<UtcTime> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| UtcTime(since.as_secs()))
    }
}

#[cfg(test)]
mod tests {
    use super::{FixedUtc, NoUtc, UtcClock, UtcTime};

    #[test]
    fn scripted_time_is_shared_settable_and_can_be_unknown() {
        let clock = FixedUtc::new(UtcTime::from_unix_seconds(100));
        let clone = clock.clone();
        clock.advance(5);
        assert_eq!(clone.now_utc(), Some(UtcTime::from_unix_seconds(105)));
        clone.set(UtcTime::from_unix_seconds(7));
        assert_eq!(clock.now_utc(), Some(UtcTime::from_unix_seconds(7)));
        clock.forget();
        assert_eq!(clone.now_utc(), None);
        clock.advance(1);
        assert_eq!(clone.now_utc(), None, "an unknown time stays unknown");
        assert_eq!(NoUtc.now_utc(), None);
        assert_eq!(
            UtcTime::from_unix_seconds(3).minus(5),
            UtcTime::from_unix_seconds(0)
        );
        assert_eq!(
            UtcTime::from_unix_seconds(u64::MAX).plus(1),
            UtcTime::from_unix_seconds(u64::MAX)
        );
    }
}
