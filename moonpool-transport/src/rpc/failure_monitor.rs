//! FailureMonitor: Reactive failure tracking for addresses and endpoints.
//!
//! Tracks two levels of failure:
//! - **Address-level**: Is a remote machine reachable? (Missing = Failed)
//! - **Endpoint-level**: Is a specific endpoint permanently dead?
//!
//! Producers (connection_task) call [`FailureMonitor::set_status`] and
//! [`FailureMonitor::notify_disconnect`]. Consumers (delivery mode functions)
//! poll [`FailureMonitor::on_disconnect_or_failure`] to race replies against
//! disconnect signals.
//!
//! # FDB Reference
//! `SimpleFailureMonitor` from `FailureMonitor.h:146`, `FailureMonitor.actor.cpp`

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::rc::Rc;
use std::task::{Poll, Waker};

use crate::Endpoint;
use moonpool_sim::{assert_always, assert_reachable, assert_sometimes};

/// Maximum number of permanently failed endpoints before clearing the map.
///
/// Matches FDB: `failedEndpoints.size() > 100000` triggers clear.
const MAX_FAILED_ENDPOINTS: usize = 100_000;

/// Status of a network address or endpoint.
///
/// # FDB Reference
/// `FailureStatus` from `FailureMonitor.h:34-60`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStatus {
    /// Address is reachable.
    Available,
    /// Address is unreachable (default for unknown addresses).
    Failed,
}

/// Reason an endpoint was permanently marked as failed.
///
/// # FDB Reference
/// `FailedReason` from `FailureMonitor.h:65-68`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailedReason {
    /// The endpoint was not found on the remote machine.
    NotFound,
}

/// Reactive failure monitor for address and endpoint tracking.
///
/// Single-threaded (`!Send`, `!Sync`) — uses `RefCell` for interior mutability.
/// Consumers register wakers via `on_disconnect_or_failure` and similar methods;
/// producers wake them via `set_status`, `notify_disconnect`, `endpoint_not_found`.
///
/// # FDB Reference
/// `SimpleFailureMonitor` from `FailureMonitor.h:146`, `FailureMonitor.actor.cpp`
pub struct FailureMonitor {
    inner: RefCell<FailureMonitorInner>,
}

struct FailureMonitorInner {
    /// Address-level status. Missing entry = Failed (FDB default).
    address_status: BTreeMap<String, FailureStatus>,
    /// Permanently failed endpoints (e.g., endpoint not found on remote).
    failed_endpoints: BTreeMap<Endpoint, FailedReason>,
    /// Wakers waiting for endpoint state changes, keyed by address.
    /// Woken on: set_status change, notify_disconnect, endpoint_not_found.
    endpoint_watchers: BTreeMap<String, Vec<Waker>>,
    /// Wakers waiting for disconnect events, keyed by address.
    /// Woken only on: notify_disconnect.
    disconnect_watchers: BTreeMap<String, Vec<Waker>>,
}

impl FailureMonitor {
    /// Create a new failure monitor with empty state.
    ///
    /// All unknown addresses default to [`FailureStatus::Failed`] until
    /// a connection succeeds and calls [`set_status`](Self::set_status).
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(FailureMonitorInner {
                address_status: BTreeMap::new(),
                failed_endpoints: BTreeMap::new(),
                endpoint_watchers: BTreeMap::new(),
                disconnect_watchers: BTreeMap::new(),
            }),
        }
    }

    // =========================================================================
    // Producer methods (called by connection_task)
    // =========================================================================

    /// Update the status of an address.
    ///
    /// Called by `connection_task` on successful connect (`Available`) or
    /// connection failure (`Failed`).
    ///
    /// Wakes all endpoint watchers for this address on status change.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::setStatus` (FailureMonitor.actor.cpp:83-115)
    pub fn set_status(&self, address: &str, status: FailureStatus) {
        let mut inner = self.inner.borrow_mut();

        let changed = match status {
            FailureStatus::Available => {
                let prev = inner.address_status.insert(address.to_string(), status);
                prev != Some(FailureStatus::Available)
            }
            FailureStatus::Failed => {
                // Missing = Failed, so remove the entry
                inner.address_status.remove(address).is_some()
            }
        };

        if changed {
            wake_all(&mut inner.endpoint_watchers, address);
        }
    }

    /// Signal that a connection to the given address has been lost.
    ///
    /// Wakes both endpoint watchers and disconnect watchers for this address.
    /// Called by `connection_task` after `disconnect_notify.notify_waiters()`.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::notifyDisconnect` (FailureMonitor.actor.cpp:150-154)
    pub fn notify_disconnect(&self, address: &str) {
        let mut inner = self.inner.borrow_mut();
        wake_all(&mut inner.endpoint_watchers, address);
        wake_all(&mut inner.disconnect_watchers, address);
    }

    /// Mark an endpoint as permanently failed (e.g., not found on remote).
    ///
    /// Permanently failed endpoints are never automatically recovered.
    /// Wakes endpoint watchers for the endpoint's address.
    ///
    /// Skips well-known tokens (system endpoints that always exist).
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::endpointNotFound` (FailureMonitor.actor.cpp:117-139)
    pub fn endpoint_not_found(&self, endpoint: &Endpoint) {
        // Skip well-known tokens (FDB: `if token.first() == -1 return`)
        if endpoint.token.is_well_known() {
            return;
        }

        let mut inner = self.inner.borrow_mut();

        // Cap to prevent memory leaks in long-running simulations
        let max = if moonpool_sim::buggify_with_prob!(0.01) {
            assert_reachable!("buggified_low_failed_endpoint_cap");
            10
        } else {
            MAX_FAILED_ENDPOINTS
        };
        if inner.failed_endpoints.len() >= max {
            tracing::warn!(
                "FailureMonitor: evicting transient failed endpoints (cap {} reached, {} entries)",
                max,
                inner.failed_endpoints.len()
            );
            // Preserve permanently failed endpoints, clear transient ones
            let permanent: Vec<_> = inner
                .failed_endpoints
                .iter()
                .filter(|(_, reason)| matches!(reason, FailedReason::NotFound))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            inner.failed_endpoints.clear();
            for (k, v) in permanent {
                inner.failed_endpoints.insert(k, v);
            }
            assert_always!(
                inner
                    .failed_endpoints
                    .values()
                    .all(|r| matches!(r, FailedReason::NotFound)),
                "failed_endpoints_preserves_permanent_after_eviction"
            );
        }

        inner
            .failed_endpoints
            .insert(endpoint.clone(), FailedReason::NotFound);

        let address = endpoint.address.to_string();
        wake_all(&mut inner.endpoint_watchers, &address);
    }

    // =========================================================================
    // Consumer methods (used by delivery mode functions)
    // =========================================================================

    /// Get the current failure status of an endpoint.
    ///
    /// Returns [`FailureStatus::Failed`] if:
    /// - The endpoint is permanently failed, OR
    /// - The endpoint's address is unknown or failed
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::getState(Endpoint)` (FailureMonitor.actor.cpp:196-206)
    pub fn state(&self, endpoint: &Endpoint) -> FailureStatus {
        let inner = self.inner.borrow();

        if inner.failed_endpoints.contains_key(endpoint) {
            return FailureStatus::Failed;
        }

        let address = endpoint.address.to_string();
        inner
            .address_status
            .get(&address)
            .copied()
            .unwrap_or(FailureStatus::Failed) // Missing = Failed
    }

    /// Get the current failure status of an address.
    ///
    /// Returns [`FailureStatus::Failed`] if the address is unknown.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::getState(NetworkAddress)` (FailureMonitor.actor.cpp:208-214)
    pub fn address_state(&self, address: &str) -> FailureStatus {
        self.inner
            .borrow()
            .address_status
            .get(address)
            .copied()
            .unwrap_or(FailureStatus::Failed)
    }

    /// Check if an endpoint is permanently failed.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::permanentlyFailed` (FailureMonitor.h:226-228)
    pub fn permanently_failed(&self, endpoint: &Endpoint) -> bool {
        self.inner.borrow().failed_endpoints.contains_key(endpoint)
    }

    /// Returns a future that resolves when the endpoint's address disconnects
    /// or the endpoint becomes permanently failed.
    ///
    /// **Fast path**: Returns `Ready` immediately if already failed.
    ///
    /// Used by `try_get_reply()` to race reply vs disconnect.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::onDisconnectOrFailure` (FailureMonitor.actor.cpp:156-178)
    pub fn on_disconnect_or_failure(
        self: &Rc<Self>,
        endpoint: &Endpoint,
    ) -> impl Future<Output = ()> {
        let fm = Rc::clone(self);
        let address = endpoint.address.to_string();
        let endpoint = endpoint.clone();

        std::future::poll_fn(move |cx| {
            let inner = fm.inner.borrow();

            // Fast path: already failed
            if inner.failed_endpoints.contains_key(&endpoint) {
                return Poll::Ready(());
            }
            if !inner
                .address_status
                .get(&address)
                .is_some_and(|s| *s == FailureStatus::Available)
            {
                // Missing or Failed → already failed
                return Poll::Ready(());
            }

            // Slow path: register waker
            assert_sometimes!(true, "failure_monitor_waker_registered_slow_path");
            drop(inner);
            let mut inner = fm.inner.borrow_mut();
            inner
                .endpoint_watchers
                .entry(address.clone())
                .or_default()
                .push(cx.waker().clone());
            Poll::Pending
        })
    }

    /// Returns a future that resolves when the endpoint's state changes.
    ///
    /// If the endpoint is permanently failed, the future never resolves.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::onStateChanged` (FailureMonitor.actor.cpp:184-194)
    pub fn on_state_changed(self: &Rc<Self>, endpoint: &Endpoint) -> impl Future<Output = ()> {
        let fm = Rc::clone(self);
        let address = endpoint.address.to_string();
        let endpoint = endpoint.clone();
        let registered = Rc::new(std::cell::Cell::new(false));

        std::future::poll_fn(move |cx| {
            let inner = fm.inner.borrow();

            // Permanently failed → never resolves (FDB: onStateChanged returns Never)
            if inner.failed_endpoints.contains_key(&endpoint) {
                return Poll::Pending;
            }

            // If we already registered and got woken, a state change happened
            if registered.get() {
                return Poll::Ready(());
            }

            // First poll: register waker
            registered.set(true);
            drop(inner);
            let mut inner = fm.inner.borrow_mut();
            inner
                .endpoint_watchers
                .entry(address.clone())
                .or_default()
                .push(cx.waker().clone());
            Poll::Pending
        })
    }

    /// Returns a future that resolves when the given address disconnects.
    ///
    /// Only triggered by explicit disconnect events, not status changes.
    ///
    /// # FDB Reference
    /// `SimpleFailureMonitor::onDisconnect` (FailureMonitor.actor.cpp:180-182)
    pub fn on_disconnect(self: &Rc<Self>, address: &str) -> impl Future<Output = ()> {
        let fm = Rc::clone(self);
        let address = address.to_string();
        let registered = Rc::new(std::cell::Cell::new(false));

        std::future::poll_fn(move |cx| {
            // If we already registered and got woken, the disconnect happened
            if registered.get() {
                return Poll::Ready(());
            }

            // First poll: register waker and mark as registered
            registered.set(true);
            let mut inner = fm.inner.borrow_mut();
            inner
                .disconnect_watchers
                .entry(address.clone())
                .or_default()
                .push(cx.waker().clone());
            Poll::Pending
        })
    }
}

impl Default for FailureMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FailureMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.borrow();
        f.debug_struct("FailureMonitor")
            .field("addresses_available", &inner.address_status.len())
            .field("endpoints_failed", &inner.failed_endpoints.len())
            .finish()
    }
}

/// Drain and wake all wakers registered for the given address.
fn wake_all(watchers: &mut BTreeMap<String, Vec<Waker>>, address: &str) {
    if let Some(wakers) = watchers.remove(address) {
        for waker in wakers {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::{NetworkAddress, UID};

    fn test_addr() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1)), 4500)
    }

    fn test_endpoint() -> Endpoint {
        Endpoint::new(test_addr(), UID::new(42, 1))
    }

    #[test]
    fn test_unknown_address_is_failed() {
        let fm = FailureMonitor::new();
        let ep = test_endpoint();
        assert_eq!(fm.state(&ep), FailureStatus::Failed);
        assert_eq!(fm.address_state("10.0.1.1:4500"), FailureStatus::Failed);
    }

    #[test]
    fn test_set_status_available() {
        let fm = FailureMonitor::new();
        let ep = test_endpoint();
        fm.set_status("10.0.1.1:4500", FailureStatus::Available);
        assert_eq!(fm.state(&ep), FailureStatus::Available);
        assert_eq!(fm.address_state("10.0.1.1:4500"), FailureStatus::Available);
    }

    #[test]
    fn test_set_status_failed_removes_entry() {
        let fm = FailureMonitor::new();
        fm.set_status("10.0.1.1:4500", FailureStatus::Available);
        fm.set_status("10.0.1.1:4500", FailureStatus::Failed);
        assert_eq!(fm.address_state("10.0.1.1:4500"), FailureStatus::Failed);
    }

    #[test]
    fn test_endpoint_not_found_marks_permanent() {
        let fm = FailureMonitor::new();
        let ep = test_endpoint();
        fm.set_status("10.0.1.1:4500", FailureStatus::Available);
        fm.endpoint_not_found(&ep);
        assert!(fm.permanently_failed(&ep));
        assert_eq!(fm.state(&ep), FailureStatus::Failed);
    }

    #[test]
    fn test_endpoint_not_found_skips_well_known() {
        let fm = FailureMonitor::new();
        let ep = Endpoint::well_known(test_addr(), crate::WellKnownToken::Ping);
        fm.endpoint_not_found(&ep);
        assert!(!fm.permanently_failed(&ep));
    }

    #[tokio::test]
    async fn test_on_disconnect_or_failure_fast_path_unknown() {
        let fm = Rc::new(FailureMonitor::new());
        let ep = test_endpoint();
        // Unknown address → already failed → should resolve immediately
        fm.on_disconnect_or_failure(&ep).await;
    }

    #[tokio::test]
    async fn test_on_disconnect_or_failure_fast_path_permanent() {
        let fm = Rc::new(FailureMonitor::new());
        let ep = test_endpoint();
        fm.set_status("10.0.1.1:4500", FailureStatus::Available);
        fm.endpoint_not_found(&ep);
        // Permanently failed → should resolve immediately
        fm.on_disconnect_or_failure(&ep).await;
    }

    #[test]
    fn test_on_disconnect_or_failure_wakes_on_status_change() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");
        rt.block_on(async {
            let fm = Rc::new(FailureMonitor::new());
            let ep = test_endpoint();
            fm.set_status("10.0.1.1:4500", FailureStatus::Available);

            let fm2 = Rc::clone(&fm);
            let handle = tokio::task::spawn_local(async move {
                fm2.on_disconnect_or_failure(&ep).await;
            });

            // Yield to let the future register its waker
            tokio::task::yield_now().await;

            // Trigger disconnect → should wake the future
            fm.set_status("10.0.1.1:4500", FailureStatus::Failed);

            handle.await.expect("task should complete");
        });
    }

    #[test]
    fn test_on_disconnect_wakes_on_notify() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");
        rt.block_on(async {
            let fm = Rc::new(FailureMonitor::new());
            fm.set_status("10.0.1.1:4500", FailureStatus::Available);

            let fm2 = Rc::clone(&fm);
            let handle = tokio::task::spawn_local(async move {
                fm2.on_disconnect("10.0.1.1:4500").await;
            });

            tokio::task::yield_now().await;

            fm.notify_disconnect("10.0.1.1:4500");

            handle.await.expect("task should complete");
        });
    }

    #[test]
    fn test_debug_impl() {
        let fm = FailureMonitor::new();
        fm.set_status("10.0.1.1:4500", FailureStatus::Available);
        let debug = format!("{:?}", fm);
        assert!(debug.contains("FailureMonitor"));
        assert!(debug.contains("addresses_available: 1"));
    }

    #[test]
    fn test_default_impl() {
        let fm = FailureMonitor::default();
        assert_eq!(fm.address_state("any"), FailureStatus::Failed);
    }
}
