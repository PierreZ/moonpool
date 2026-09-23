//! The failure monitor's facts: per-address availability and disconnect
//! counts, and the bounded set of endpoints known permanently failed.
//!
//! Plain data owned by the runtime's state and mutated under its lock; the
//! caller publishes every change through the runtime's
//! [`Watch`](super::watch::Watch) after unlocking.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use super::{AddressState, EndpointState};
use crate::endpoint::{Endpoint, Incarnation};

/// Why an endpoint is known failed for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermanentFailure {
    /// The endpoint was destroyed (or never existed).
    NotFound,
    /// Its whole runtime incarnation is gone from that address.
    StaleIncarnation,
}

/// What is remembered as failed: one endpoint, or every endpoint of one
/// incarnation at one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FailedKey {
    Endpoint(Endpoint),
    Incarnation(SocketAddr, Incarnation),
}

#[derive(Debug, Clone, Default)]
struct AddressRecord {
    failed: bool,
    disconnects: u64,
    /// The runtime-wide disconnect sequence of this address's latest
    /// disconnect (0: none since the record was created).
    last_disconnect: u64,
    /// When connections to the address started failing without a success
    /// since.
    failing_since: Option<Duration>,
    /// Live watchers that want the address probed while it is failing.
    references: usize,
}

/// See the module docs.
#[derive(Debug)]
pub(crate) struct MonitorState {
    addresses: BTreeMap<SocketAddr, AddressRecord>,
    failed: BTreeMap<FailedKey, PermanentFailure>,
    /// Insertion order of `failed`, oldest first.
    order: VecDeque<FailedKey>,
    max_failed: usize,
    max_addresses: usize,
    /// Runtime-wide, monotonic: bumped on every disconnect of any address.
    /// Waiters compare against it, so forgetting an address record never
    /// hides a later disconnect.
    sequence: u64,
}

impl MonitorState {
    pub(crate) fn new(max_failed: usize, max_addresses: usize) -> Self {
        Self {
            addresses: BTreeMap::new(),
            failed: BTreeMap::new(),
            order: VecDeque::new(),
            max_failed: max_failed.max(1),
            max_addresses: max_addresses.max(1),
            sequence: 0,
        }
    }

    /// Whether `address` is failed. An address never seen is available.
    pub(crate) fn address_state(&self, address: SocketAddr) -> AddressState {
        match self.addresses.get(&address) {
            Some(record) if record.failed => AddressState::Failed,
            _ => AddressState::Available,
        }
    }

    /// The runtime-wide disconnect sequence so far.
    pub(crate) fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether `address` disconnected after the sequence value `since`.
    pub(crate) fn disconnected_since(&self, address: SocketAddr, since: u64) -> bool {
        self.addresses
            .get(&address)
            .is_some_and(|record| record.last_disconnect > since)
    }

    /// Disconnects of `address` observed since its record was created (a
    /// forgotten address starts again from zero).
    pub(crate) fn disconnects(&self, address: SocketAddr) -> u64 {
        self.addresses
            .get(&address)
            .map_or(0, |record| record.disconnects)
    }

    /// Live watchers of `address`.
    pub(crate) fn references(&self, address: SocketAddr) -> usize {
        self.addresses
            .get(&address)
            .map_or(0, |record| record.references)
    }

    /// Whether `endpoint` is known failed for good (never for a well-known
    /// endpoint, which may come back at the same token).
    pub(crate) fn permanent_failure(&self, endpoint: &Endpoint) -> Option<PermanentFailure> {
        if endpoint.token().is_well_known() {
            return None;
        }
        self.failed
            .get(&FailedKey::Endpoint(*endpoint))
            .or_else(|| {
                self.failed.get(&FailedKey::Incarnation(
                    endpoint.address(),
                    endpoint.incarnation(),
                ))
            })
            .copied()
    }

    /// The endpoint's state: permanent failures first, then its address.
    pub(crate) fn endpoint_state(&self, endpoint: &Endpoint) -> EndpointState {
        match self.permanent_failure(endpoint) {
            Some(PermanentFailure::NotFound) => EndpointState::NotFound,
            Some(PermanentFailure::StaleIncarnation) => EndpointState::StaleIncarnation,
            None => match self.address_state(endpoint.address()) {
                AddressState::Failed => EndpointState::AddressFailed,
                AddressState::Available => EndpointState::Available,
            },
        }
    }

    fn record(&mut self, address: SocketAddr) -> &mut AddressRecord {
        if !self.addresses.contains_key(&address) && self.addresses.len() >= self.max_addresses {
            // Forget an available, unwatched address: it answers the same
            // (available, no failure run) once it is gone. Its disconnect
            // count restarts; waiters compare the runtime-wide sequence,
            // which never goes back, so none of them misses a disconnect.
            let victim = self
                .addresses
                .iter()
                .find(|(_, record)| {
                    !record.failed && record.references == 0 && record.failing_since.is_none()
                })
                .map(|(address, _)| *address);
            if let Some(victim) = victim {
                self.addresses.remove(&victim);
            }
        }
        self.addresses.entry(address).or_default()
    }

    /// A session with `address` established: it is available and its
    /// failure run ended. Returns whether the observable state changed.
    pub(crate) fn connected(&mut self, address: SocketAddr) -> bool {
        let record = self.record(address);
        record.failing_since = None;
        std::mem::replace(&mut record.failed, false)
    }

    /// A selected connection to `address` ended (or never came up). Every
    /// such end is a disconnect event; a failure additionally extends the
    /// failure run and marks the address failed once it lasted `delay`.
    pub(crate) fn disconnected(
        &mut self,
        address: SocketAddr,
        failure: bool,
        now: Duration,
        delay: Duration,
    ) {
        self.sequence += 1;
        let sequence = self.sequence;
        let record = self.record(address);
        record.disconnects += 1;
        record.last_disconnect = sequence;
        if failure {
            let since = *record.failing_since.get_or_insert(now);
            if now.saturating_sub(since) >= delay {
                record.failed = true;
            }
        }
    }

    /// Remember that `endpoint` failed permanently for `reason`. Returns
    /// whether it was new. Well-known endpoints are never remembered.
    pub(crate) fn endpoint_failed(
        &mut self,
        endpoint: &Endpoint,
        reason: PermanentFailure,
    ) -> bool {
        if endpoint.token().is_well_known() {
            return false;
        }
        let key = match reason {
            PermanentFailure::NotFound => FailedKey::Endpoint(*endpoint),
            PermanentFailure::StaleIncarnation => {
                FailedKey::Incarnation(endpoint.address(), endpoint.incarnation())
            }
        };
        if self.failed.contains_key(&key) {
            return false;
        }
        while self.failed.len() >= self.max_failed {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.failed.remove(&oldest);
        }
        self.failed.insert(key, reason);
        self.order.push_back(key);
        true
    }

    /// A watcher of `address` starts (`true`) or stops (`false`).
    pub(crate) fn reference(&mut self, address: SocketAddr, add: bool) {
        let record = self.record(address);
        if add {
            record.references += 1;
        } else {
            record.references = record.references.saturating_sub(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn remembered_failures(&self) -> usize {
        self.failed.len()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::{MonitorState, PermanentFailure};
    use crate::endpoint::{Endpoint, EndpointToken, Incarnation, WellKnownId};
    use crate::failure::{AddressState, EndpointState};

    fn address() -> SocketAddr {
        "10.0.1.1:4500".parse().expect("literal address")
    }

    fn endpoint(index: u64, incarnation: u128) -> Endpoint {
        Endpoint::new(
            address(),
            Incarnation::from_raw(incarnation),
            EndpointToken::from_parts(index, 0),
        )
    }

    #[test]
    fn failure_waits_for_the_detection_delay_and_success_clears_it() {
        let mut monitor = MonitorState::new(8, 8);
        let delay = Duration::from_secs(4);
        monitor.disconnected(address(), true, Duration::from_secs(1), delay);
        assert_eq!(monitor.address_state(address()), AddressState::Available);
        assert_eq!(monitor.disconnects(address()), 1);
        monitor.disconnected(address(), true, Duration::from_secs(5), delay);
        assert_eq!(monitor.address_state(address()), AddressState::Failed);
        assert!(monitor.connected(address()));
        assert_eq!(monitor.address_state(address()), AddressState::Available);
        // A non-failure end (idle, tie-break) is an event, never a failure.
        monitor.disconnected(address(), false, Duration::from_secs(20), Duration::ZERO);
        assert_eq!(monitor.address_state(address()), AddressState::Available);
        assert_eq!(monitor.disconnects(address()), 3);
    }

    #[test]
    fn endpoint_failures_are_distinct_from_address_failures_and_bounded() {
        let mut monitor = MonitorState::new(2, 8);
        assert!(monitor.endpoint_failed(&endpoint(1, 7), PermanentFailure::NotFound));
        assert!(!monitor.endpoint_failed(&endpoint(1, 7), PermanentFailure::NotFound));
        assert_eq!(
            monitor.endpoint_state(&endpoint(1, 7)),
            EndpointState::NotFound
        );
        // The address stays available: only that endpoint is gone.
        assert_eq!(
            monitor.endpoint_state(&endpoint(2, 7)),
            EndpointState::Available
        );
        // A stale incarnation fails every endpoint of it at that address.
        assert!(monitor.endpoint_failed(&endpoint(3, 8), PermanentFailure::StaleIncarnation));
        assert_eq!(
            monitor.endpoint_state(&endpoint(9, 8)),
            EndpointState::StaleIncarnation
        );
        // Bounded: the oldest failure is forgotten first.
        assert!(monitor.endpoint_failed(&endpoint(4, 9), PermanentFailure::NotFound));
        assert_eq!(monitor.remembered_failures(), 2);
        assert_eq!(
            monitor.endpoint_state(&endpoint(1, 7)),
            EndpointState::Available
        );
        // Well-known endpoints may come back: never remembered.
        let well_known = Endpoint::new(
            address(),
            Incarnation::from_raw(0),
            EndpointToken::well_known(WellKnownId::new(3)),
        );
        assert!(!monitor.endpoint_failed(&well_known, PermanentFailure::NotFound));
        assert_eq!(monitor.permanent_failure(&well_known), None);
    }

    #[test]
    fn a_forgotten_address_never_hides_a_later_disconnect() {
        let mut monitor = MonitorState::new(8, 2);
        let watched = address();
        let other: SocketAddr = "10.0.1.2:1".parse().expect("literal");
        let third: SocketAddr = "10.0.1.3:1".parse().expect("literal");
        monitor.disconnected(watched, false, Duration::ZERO, Duration::ZERO);
        // A waiter registers here, after one disconnect.
        let since = monitor.sequence();
        assert!(!monitor.disconnected_since(watched, since));
        // The table is full: touching two other addresses forgets it.
        let _ = monitor.connected(other);
        let _ = monitor.connected(third);
        assert_eq!(monitor.disconnects(watched), 0, "record forgotten");
        // Its next disconnect is still seen (a per-record count would read
        // 1 again, equal to what the waiter captured, and miss it).
        monitor.disconnected(watched, false, Duration::ZERO, Duration::ZERO);
        assert!(monitor.disconnected_since(watched, since));
    }

    #[test]
    fn address_tracking_is_bounded_without_forgetting_failures() {
        let mut monitor = MonitorState::new(8, 2);
        let delay = Duration::ZERO;
        let first: SocketAddr = "10.0.1.1:1".parse().expect("literal");
        let second: SocketAddr = "10.0.1.2:1".parse().expect("literal");
        let third: SocketAddr = "10.0.1.3:1".parse().expect("literal");
        monitor.disconnected(first, true, Duration::ZERO, delay);
        let _ = monitor.connected(second);
        let _ = monitor.connected(third);
        assert_eq!(monitor.address_state(first), AddressState::Failed);
        assert_eq!(monitor.disconnects(second), 0);
    }
}
