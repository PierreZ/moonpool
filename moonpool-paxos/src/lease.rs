//! Conservative leases for linearizable reads.
//!
//! In VP II, the primary holds a **time-bounded lease** that allows it to
//! serve reads locally without an additional round of consensus. When the
//! lease expires, the primary must either renew it or stop serving reads.
//!
//! ## Raft Comparison
//!
//! Raft has a similar concept called "read leases" or "leader leases":
//!
//! | Aspect | Raft | VP II |
//! |---|---|---|
//! | **Lease holder** | Leader | Primary |
//! | **Lease duration** | Election timeout | Configured duration |
//! | **Renewal** | Heartbeat ACKs | Heartbeat ACKs from master |
//! | **Safety margin** | Clock skew assumptions | max_clock_drift added |
//! | **On expiry** | Leader steps down | Primary stops serving reads |
//!
//! ## Conservative Lease Protocol
//!
//! The lease is "conservative" because the master adds a safety margin
//! (`max_clock_drift`) before electing a new primary. This ensures the
//! old primary's lease has truly expired before a new primary can start
//! serving, even if clocks are slightly skewed.
//!
//! ```text
//! Primary                              Master
//!   │                                    │
//!   │── heartbeat ─────────────────────>│
//!   │<── heartbeat_ack (lease_until) ───│  lease_until = now + lease_duration
//!   │                                    │
//!   │  (serves reads locally while      │
//!   │   now < lease_until)              │
//!   │                                    │
//!   │── heartbeat ─────────────────────>│  refreshes lease
//!   │<── heartbeat_ack (lease_until) ───│
//!   │                                    │
//!   │  ✗ (primary crashes)              │
//!   │                                    │
//!   │                                    │  wait: lease_duration + max_clock_drift
//!   │                                    │  → then elect new primary
//!   │                                    │
//! ```
//!
//! The key safety property: the master never elects a new primary while the
//! old primary's lease might still be valid. The `max_clock_drift` margin
//! accounts for clock skew between the primary and master.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Configuration for the conservative lease system.
///
/// ## Tuning Guidelines
///
/// - `lease_duration` determines how long the primary can serve reads after
///   a heartbeat ACK. Longer = fewer renewals needed, but slower failover.
///
/// - `max_clock_drift` is the maximum expected clock skew between any two
///   nodes. The master waits `lease_duration + max_clock_drift` before
///   electing a new primary. Set conservatively (100ms is typical for
///   well-configured NTP).
///
/// - In simulation, `max_clock_drift` corresponds to the simulated clock
///   drift configured in `NetworkConfiguration::max_clock_drift_ms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseConfig {
    /// How long a lease is valid after a heartbeat ACK.
    ///
    /// The primary can serve reads locally during this window.
    pub lease_duration: Duration,

    /// Maximum expected clock skew between nodes.
    ///
    /// The master adds this to `lease_duration` when waiting before
    /// electing a new primary, to ensure the old primary's lease has
    /// truly expired.
    pub max_clock_drift: Duration,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            lease_duration: Duration::from_secs(5),
            max_clock_drift: Duration::from_millis(100),
        }
    }
}

impl LeaseConfig {
    /// Create a lease config suitable for simulation testing.
    pub fn for_simulation() -> Self {
        Self {
            lease_duration: Duration::from_millis(500),
            max_clock_drift: Duration::from_millis(100),
        }
    }

    /// Total time the master must wait before electing a new primary.
    ///
    /// This is `lease_duration + max_clock_drift` — the conservative
    /// bound that ensures the old primary's lease has expired on ALL
    /// nodes, even with worst-case clock skew.
    pub fn master_wait_duration(&self) -> Duration {
        self.lease_duration + self.max_clock_drift
    }
}

/// Tracks the primary's lease state.
///
/// The primary updates `valid_until` whenever it receives a heartbeat ACK
/// from the master, and checks it before serving reads.
#[derive(Debug, Clone)]
pub struct LeaseState {
    /// The time until which the lease is valid.
    ///
    /// Stored as a `Duration` from the time provider's epoch.
    /// The primary can serve reads while `now < valid_until`.
    valid_until: Duration,

    /// Lease configuration.
    config: LeaseConfig,
}

impl LeaseState {
    /// Create a new lease state with no active lease.
    pub fn new(config: LeaseConfig) -> Self {
        Self {
            valid_until: Duration::ZERO,
            config,
        }
    }

    /// Renew the lease after receiving a heartbeat ACK.
    ///
    /// Sets `valid_until = now + lease_duration`.
    pub fn renew(&mut self, now: Duration) {
        self.valid_until = now + self.config.lease_duration;
    }

    /// Check whether the lease is currently valid.
    ///
    /// The primary should only serve reads when this returns `true`.
    pub fn is_valid(&self, now: Duration) -> bool {
        now < self.valid_until
    }

    /// Get the remaining lease duration.
    ///
    /// Returns `Duration::ZERO` if the lease has expired.
    pub fn remaining(&self, now: Duration) -> Duration {
        self.valid_until.saturating_sub(now)
    }

    /// Get the time at which the lease expires.
    pub fn valid_until(&self) -> Duration {
        self.valid_until
    }

    /// Get the lease configuration.
    pub fn config(&self) -> &LeaseConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lease_initially_invalid() {
        let lease = LeaseState::new(LeaseConfig::default());
        assert!(!lease.is_valid(Duration::from_secs(1)));
        assert_eq!(lease.remaining(Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn test_lease_renew_and_valid() {
        let mut lease = LeaseState::new(LeaseConfig {
            lease_duration: Duration::from_secs(5),
            max_clock_drift: Duration::from_millis(100),
        });

        // Renew at t=10s
        lease.renew(Duration::from_secs(10));

        // Valid until t=15s
        assert!(lease.is_valid(Duration::from_secs(10)));
        assert!(lease.is_valid(Duration::from_secs(14)));
        assert!(!lease.is_valid(Duration::from_secs(15)));
        assert!(!lease.is_valid(Duration::from_secs(16)));
    }

    #[test]
    fn test_lease_remaining() {
        let mut lease = LeaseState::new(LeaseConfig {
            lease_duration: Duration::from_secs(5),
            max_clock_drift: Duration::from_millis(100),
        });

        lease.renew(Duration::from_secs(10));

        assert_eq!(
            lease.remaining(Duration::from_secs(10)),
            Duration::from_secs(5)
        );
        assert_eq!(
            lease.remaining(Duration::from_secs(13)),
            Duration::from_secs(2)
        );
        assert_eq!(lease.remaining(Duration::from_secs(15)), Duration::ZERO);
        assert_eq!(lease.remaining(Duration::from_secs(20)), Duration::ZERO);
    }

    #[test]
    fn test_lease_renew_extends() {
        let mut lease = LeaseState::new(LeaseConfig {
            lease_duration: Duration::from_secs(5),
            max_clock_drift: Duration::from_millis(100),
        });

        // Renew at t=10s → valid until t=15s
        lease.renew(Duration::from_secs(10));
        assert_eq!(lease.valid_until(), Duration::from_secs(15));

        // Renew again at t=12s → valid until t=17s
        lease.renew(Duration::from_secs(12));
        assert_eq!(lease.valid_until(), Duration::from_secs(17));
    }

    #[test]
    fn test_master_wait_duration() {
        let config = LeaseConfig {
            lease_duration: Duration::from_secs(5),
            max_clock_drift: Duration::from_millis(100),
        };

        assert_eq!(config.master_wait_duration(), Duration::from_millis(5100));
    }

    #[test]
    fn test_simulation_config() {
        let config = LeaseConfig::for_simulation();
        assert_eq!(config.lease_duration, Duration::from_millis(500));
        assert_eq!(config.max_clock_drift, Duration::from_millis(100));
        assert_eq!(config.master_wait_duration(), Duration::from_millis(600));
    }
}
