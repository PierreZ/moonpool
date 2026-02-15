//! Failure detection via heartbeats.
//!
//! In Vertical Paxos II, the **configuration master** detects primary failures
//! by monitoring heartbeats. The active primary sends periodic heartbeats to
//! the master; if the master doesn't hear from the primary within a timeout,
//! it triggers reconfiguration.
//!
//! ## Raft Comparison
//!
//! In Raft, followers detect leader failure by election timeout — if a follower
//! doesn't receive an AppendEntries (heartbeat) within the timeout, it starts
//! an election. VP II is similar but the **master** (not followers) detects
//! failure, and reconfiguration is master-driven rather than follower-initiated.
//!
//! ```text
//! Raft:          Follower ──(timeout)──> starts election
//! VP II:         Master ──(timeout)──> triggers reconfiguration
//! ```
//!
//! ## Heartbeat Protocol
//!
//! ```text
//! Primary                         Master
//!   │                               │
//!   │── HeartbeatRequest(ballot) ──>│  periodic (every heartbeat_interval)
//!   │<── HeartbeatResponse(ok) ─────│
//!   │                               │
//!   │── HeartbeatRequest(ballot) ──>│
//!   │<── HeartbeatResponse(ok) ─────│
//!   │                               │
//!   │    ✗ (primary crashes)        │
//!   │                               │  ... heartbeat_timeout elapses ...
//!   │                               │  → master triggers reconfiguration
//! ```
//!
//! ## Configuration
//!
//! - `heartbeat_interval`: How often the primary sends heartbeats (e.g., 500ms).
//! - `heartbeat_timeout`: How long the master waits before declaring failure
//!   (e.g., 2s). Should be > 2 × heartbeat_interval to tolerate one missed beat.
//!
//! All timing uses [`TimeProvider`] so heartbeats work correctly in both
//! simulation (logical time) and production (wall clock).

use std::time::Duration;

use moonpool_core::NetworkAddress;
use serde::{Deserialize, Serialize};

use crate::types::BallotNumber;

/// Heartbeat request sent from the active primary to the master.
///
/// The primary sends this periodically to prove it's alive. The master
/// uses the absence of heartbeats to detect primary failure.
///
/// In Raft terms, this is like the leader's heartbeat (empty AppendEntries),
/// but sent to the master instead of to followers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// The primary's current ballot.
    ///
    /// The master uses this to verify the heartbeat is from the current
    /// active primary (not a stale one from a previous configuration).
    pub ballot: BallotNumber,

    /// The primary's network address.
    pub leader: NetworkAddress,
}

/// Heartbeat response from the master to the primary.
///
/// Confirms the master received the heartbeat. Also tells the primary
/// whether it's still considered the active leader (the master might
/// have already started reconfiguration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Whether the master still considers this primary as the active leader.
    ///
    /// `false` means the master has moved to a new configuration, and the
    /// primary should step down. In Raft terms, this is like receiving a
    /// response with a higher term — the leader must step down.
    pub still_leader: bool,
}

/// Configuration for the heartbeat / failure detection system.
///
/// ## Tuning Guidelines
///
/// - `heartbeat_interval` should be frequent enough to provide timely failure
///   detection but not so frequent as to waste bandwidth. A typical value is
///   500ms–1s.
///
/// - `heartbeat_timeout` should be > 2× `heartbeat_interval` to tolerate one
///   or two missed heartbeats (due to network jitter, GC pauses, etc.) without
///   falsely declaring the primary dead.
///
/// - In simulation, these values can be much smaller (e.g., 100ms / 300ms)
///   since logical time has no jitter.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// How often the primary sends heartbeats to the master.
    ///
    /// In Raft terms, this is like the heartbeat interval (typically 50-150ms
    /// in Raft, but VP II can be more relaxed since the master does detection).
    pub heartbeat_interval: Duration,

    /// How long the master waits without a heartbeat before declaring failure.
    ///
    /// In Raft terms, this is like the election timeout on followers.
    /// Must be > 2 × `heartbeat_interval`.
    pub heartbeat_timeout: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_millis(500),
            heartbeat_timeout: Duration::from_secs(2),
        }
    }
}

impl HeartbeatConfig {
    /// Create a heartbeat config suitable for simulation testing.
    ///
    /// Uses shorter intervals since simulation uses logical time.
    pub fn for_simulation() -> Self {
        Self {
            heartbeat_interval: Duration::from_millis(100),
            heartbeat_timeout: Duration::from_millis(300),
        }
    }
}

/// Tracks the last heartbeat time for a primary.
///
/// Used by the master to detect primary failure. The master updates
/// `last_heartbeat` whenever it receives a valid heartbeat from the
/// active primary, and checks for timeout periodically.
#[derive(Debug, Clone)]
pub struct HeartbeatTracker {
    /// Ballot of the currently tracked primary.
    pub ballot: BallotNumber,

    /// Address of the currently tracked primary.
    pub leader: NetworkAddress,

    /// Logical time of the last received heartbeat.
    ///
    /// Stored as a `Duration` from the time provider's epoch. We use
    /// `Duration` (not `Instant`) because simulation uses logical time
    /// that can be compared as durations.
    pub last_heartbeat: Duration,

    /// Configuration for timeout detection.
    pub config: HeartbeatConfig,
}

impl HeartbeatTracker {
    /// Create a new tracker for the given primary.
    ///
    /// `now` is the current time from the time provider.
    pub fn new(
        ballot: BallotNumber,
        leader: NetworkAddress,
        now: Duration,
        config: HeartbeatConfig,
    ) -> Self {
        Self {
            ballot,
            leader,
            last_heartbeat: now,
            config,
        }
    }

    /// Record a heartbeat from the primary.
    ///
    /// Returns `true` if the heartbeat was accepted (correct ballot),
    /// `false` if it was from a stale ballot.
    pub fn record_heartbeat(&mut self, ballot: BallotNumber, now: Duration) -> bool {
        if ballot != self.ballot {
            return false;
        }
        self.last_heartbeat = now;
        true
    }

    /// Check whether the primary has timed out.
    ///
    /// Returns `true` if the time since the last heartbeat exceeds
    /// `heartbeat_timeout`, indicating the primary is likely dead.
    pub fn is_timed_out(&self, now: Duration) -> bool {
        now.saturating_sub(self.last_heartbeat) > self.config.heartbeat_timeout
    }

    /// Get the duration since the last heartbeat.
    pub fn time_since_last(&self, now: Duration) -> Duration {
        now.saturating_sub(self.last_heartbeat)
    }
}

/// RPC interface ID for the heartbeat service on the master.
pub const HEARTBEAT_INTERFACE_ID: u64 = 0xBEA7_0001;

/// Method index for the heartbeat RPC.
pub const HEARTBEAT_METHOD: u64 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port)
    }

    #[test]
    fn test_heartbeat_tracker_new() {
        let tracker = HeartbeatTracker::new(
            BallotNumber::new(1),
            make_addr(5001),
            Duration::from_secs(10),
            HeartbeatConfig::for_simulation(),
        );

        assert_eq!(tracker.ballot, BallotNumber::new(1));
        assert_eq!(tracker.last_heartbeat, Duration::from_secs(10));
    }

    #[test]
    fn test_heartbeat_record_valid() {
        let mut tracker = HeartbeatTracker::new(
            BallotNumber::new(1),
            make_addr(5001),
            Duration::from_secs(10),
            HeartbeatConfig::for_simulation(),
        );

        let accepted = tracker.record_heartbeat(BallotNumber::new(1), Duration::from_secs(11));
        assert!(accepted);
        assert_eq!(tracker.last_heartbeat, Duration::from_secs(11));
    }

    #[test]
    fn test_heartbeat_record_stale_ballot() {
        let mut tracker = HeartbeatTracker::new(
            BallotNumber::new(2),
            make_addr(5001),
            Duration::from_secs(10),
            HeartbeatConfig::for_simulation(),
        );

        // Heartbeat from old ballot should be rejected
        let accepted = tracker.record_heartbeat(BallotNumber::new(1), Duration::from_secs(11));
        assert!(!accepted);
        // last_heartbeat should NOT be updated
        assert_eq!(tracker.last_heartbeat, Duration::from_secs(10));
    }

    #[test]
    fn test_heartbeat_timeout_detection() {
        let config = HeartbeatConfig {
            heartbeat_interval: Duration::from_millis(100),
            heartbeat_timeout: Duration::from_millis(300),
        };

        let tracker = HeartbeatTracker::new(
            BallotNumber::new(1),
            make_addr(5001),
            Duration::from_secs(10),
            config,
        );

        // 200ms later: not timed out (300ms timeout)
        assert!(!tracker.is_timed_out(Duration::from_millis(10_200)));

        // 300ms later: on the boundary (not timed out, needs to exceed)
        assert!(!tracker.is_timed_out(Duration::from_millis(10_300)));

        // 301ms later: timed out!
        assert!(tracker.is_timed_out(Duration::from_millis(10_301)));
    }

    #[test]
    fn test_heartbeat_timeout_reset() {
        let config = HeartbeatConfig {
            heartbeat_interval: Duration::from_millis(100),
            heartbeat_timeout: Duration::from_millis(300),
        };

        let mut tracker = HeartbeatTracker::new(
            BallotNumber::new(1),
            make_addr(5001),
            Duration::from_secs(10),
            config,
        );

        // At 10.2s: record heartbeat, resets the timeout
        tracker.record_heartbeat(BallotNumber::new(1), Duration::from_millis(10_200));

        // At 10.4s: only 200ms since last heartbeat, not timed out
        assert!(!tracker.is_timed_out(Duration::from_millis(10_400)));

        // At 10.501s: 301ms since last heartbeat, timed out
        assert!(tracker.is_timed_out(Duration::from_millis(10_501)));
    }

    #[test]
    fn test_time_since_last() {
        let tracker = HeartbeatTracker::new(
            BallotNumber::new(1),
            make_addr(5001),
            Duration::from_secs(10),
            HeartbeatConfig::default(),
        );

        assert_eq!(
            tracker.time_since_last(Duration::from_secs(12)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn test_default_config() {
        let config = HeartbeatConfig::default();
        assert_eq!(config.heartbeat_interval, Duration::from_millis(500));
        assert_eq!(config.heartbeat_timeout, Duration::from_secs(2));
        // Timeout should be > 2× interval
        assert!(config.heartbeat_timeout > config.heartbeat_interval * 2);
    }

    #[test]
    fn test_simulation_config() {
        let config = HeartbeatConfig::for_simulation();
        assert_eq!(config.heartbeat_interval, Duration::from_millis(100));
        assert_eq!(config.heartbeat_timeout, Duration::from_millis(300));
        // Timeout should be > 2× interval
        assert!(config.heartbeat_timeout > config.heartbeat_interval * 2);
    }
}
