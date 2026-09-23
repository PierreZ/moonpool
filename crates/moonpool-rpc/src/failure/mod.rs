//! The failure monitor: what one runtime currently believes about the
//! addresses and endpoints it talks to, and race-free ways to wait for that
//! belief to change.
//!
//! Three facts are kept apart on purpose (`FoundationDB`'s
//! `SimpleFailureMonitor`, without its process-global singleton):
//!
//! - **Address availability** ([`AddressState`]). An address becomes
//!   [`Failed`](AddressState::Failed) only after connections to it kept
//!   failing for [`PeerPolicy::failure_detection_delay`](crate::PeerPolicy::failure_detection_delay),
//!   and [`Available`](AddressState::Available) again as soon as a session
//!   with it establishes. An address never contacted is available.
//! - **Disconnect events.** Every end of a connection this runtime used to
//!   call an address is stamped at once with a runtime-wide, monotonic
//!   sequence, even while the address still
//!   counts as available: a call bound to the connection that died learns
//!   of it without waiting for the failure delay.
//! - **Permanent endpoint failure** ([`EndpointState::NotFound`],
//!   [`EndpointState::StaleIncarnation`]). Learned from the server's own
//!   rejection and remembered (bounded), so later calls to the same dynamic
//!   reference fail at once without a round trip. A live address never
//!   revives a dead dynamic endpoint; well-known endpoints are never
//!   remembered as failed because they may come back at the same token.
//!
//! Every wait registers before it checks (a versioned latch), so a change
//! that happens between the check and the wait is never lost. Waits may
//! wake spuriously; each re-checks its condition. All of them fail with
//! [`ErrorReason::Shutdown`] once the runtime is gone.

mod state;
pub(crate) mod watch;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use moonpool_core::{Providers, TimeProvider};

pub(crate) use self::state::{MonitorState, PermanentFailure};
use self::watch::Watch;
use crate::endpoint::Endpoint;
use crate::error::{ErrorReason, RpcError};
use crate::transport::Shared;

/// Whether an address is believed reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AddressState {
    /// Reachable, or never contacted, or failing for less than the
    /// detection delay.
    Available,
    /// Connections kept failing for at least the detection delay.
    Failed,
}

/// Whether an endpoint is believed callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum EndpointState {
    /// Nothing known against it.
    Available,
    /// Its address is failed; the endpoint may still exist.
    AddressFailed,
    /// The server said the endpoint does not exist. Permanent for a
    /// dynamic reference.
    NotFound,
    /// The server at that address is a later incarnation. Permanent.
    StaleIncarnation,
}

impl EndpointState {
    /// Whether the state can never change back to available.
    #[must_use]
    pub fn is_permanent(self) -> bool {
        matches!(self, Self::NotFound | Self::StaleIncarnation)
    }
}

/// A weak handle to one runtime's failure monitor.
///
/// Obtained from [`RpcHandle::failure_monitor`](crate::RpcHandle::failure_monitor).
/// Queries answer from the monitor's current belief; `on_*` methods return
/// futures that resolve when that belief changes. Each future captures
/// what it compares against when it is *created*, not when first polled.
pub struct FailureMonitor<P: Providers> {
    shared: Weak<Shared<P>>,
    watch: Arc<Watch>,
    time: P::Time,
}

impl<P: Providers> Clone for FailureMonitor<P> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            watch: Arc::clone(&self.watch),
            time: self.time.clone(),
        }
    }
}

impl<P: Providers> std::fmt::Debug for FailureMonitor<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailureMonitor")
            .field("revision", &self.revision())
            .finish_non_exhaustive()
    }
}

/// Cap on one sustained-failure wait, so the timer arithmetic stays finite.
const LONGEST_WAIT: Duration = Duration::from_hours(365 * 24);

fn shutdown() -> RpcError {
    RpcError::not_admitted(ErrorReason::Shutdown)
}

impl<P: Providers> FailureMonitor<P> {
    pub(crate) fn new(shared: &Arc<Shared<P>>) -> Self {
        Self {
            shared: Arc::downgrade(shared),
            watch: Arc::clone(shared.watch()),
            time: shared.time().clone(),
        }
    }

    fn read<T: Default>(&self, read: impl FnOnce(&MonitorState) -> T) -> T {
        self.shared
            .upgrade()
            .map(|shared| shared.read_monitor(read))
            .unwrap_or_default()
    }

    /// The monitor's revision: it moves on every published change.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.watch.version()
    }

    /// Whether `address` is believed reachable ([`AddressState::Available`]
    /// once the runtime is gone).
    #[must_use]
    pub fn address_state(&self, address: SocketAddr) -> AddressState {
        self.shared
            .upgrade()
            .map_or(AddressState::Available, |shared| {
                shared.read_monitor(|monitor| monitor.address_state(address))
            })
    }

    /// Whether `endpoint` is believed callable.
    #[must_use]
    pub fn endpoint_state(&self, endpoint: &Endpoint) -> EndpointState {
        self.shared
            .upgrade()
            .map_or(EndpointState::Available, |shared| {
                shared.read_monitor(|monitor| monitor.endpoint_state(endpoint))
            })
    }

    /// Disconnects of `address` observed so far. The count restarts from
    /// zero if the monitor forgot the address (see
    /// [`PeerPolicy::max_tracked_addresses`](crate::PeerPolicy::max_tracked_addresses));
    /// the `on_*` waits do not depend on it.
    #[must_use]
    pub fn disconnects(&self, address: SocketAddr) -> u64 {
        self.read(|monitor| monitor.disconnects(address))
    }

    /// Resolves once `ready` holds, checked after registering for changes.
    fn wait_until(
        &self,
        mut ready: impl FnMut(&MonitorState) -> bool + Send + 'static,
    ) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        let shared = self.shared.clone();
        let watch = Arc::clone(&self.watch);
        async move {
            loop {
                // Register (read the version) before checking the state.
                let version = watch.version();
                let satisfied = shared
                    .upgrade()
                    .ok_or_else(shutdown)?
                    .read_monitor(&mut ready);
                if satisfied {
                    return Ok(());
                }
                // The wait holds only the latch, never the runtime.
                if !watch.changed(version).await {
                    return Err(shutdown());
                }
            }
        }
    }

    /// Resolves at the monitor's next published change after this call.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn on_change(&self) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        let watch = Arc::clone(&self.watch);
        let since = watch.version();
        async move {
            if watch.changed(since).await {
                Ok(())
            } else {
                Err(shutdown())
            }
        }
    }

    /// Resolves at the next disconnect of `address` after this call.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn on_disconnect(
        &self,
        address: SocketAddr,
    ) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        let since = self.read(MonitorState::sequence);
        self.wait_until(move |monitor| monitor.disconnected_since(address, since))
    }

    /// Resolves once `endpoint` is not [`EndpointState::Available`]: its
    /// address failed or it failed permanently.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn on_failed(
        &self,
        endpoint: Endpoint,
    ) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        self.wait_until(move |monitor| {
            monitor.endpoint_state(&endpoint) != EndpointState::Available
        })
    }

    /// Resolves once `address` is [`AddressState::Available`].
    ///
    /// # Errors
    ///
    /// [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn on_available(
        &self,
        address: SocketAddr,
    ) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        self.wait_until(move |monitor| monitor.address_state(address) == AddressState::Available)
    }

    /// Resolves at the next disconnect of the endpoint's address after this
    /// call, or at once if the endpoint is already failed (`FoundationDB`'s
    /// `onDisconnectOrFailure`).
    ///
    /// # Errors
    ///
    /// [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn on_disconnect_or_failure(
        &self,
        endpoint: Endpoint,
    ) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        let since = self.read(MonitorState::sequence);
        self.wait_until(move |monitor| {
            monitor.endpoint_state(&endpoint) != EndpointState::Available
                || monitor.disconnected_since(endpoint.address(), since)
        })
    }

    /// Resolves once `endpoint` failed permanently, or its address has been
    /// failed continuously for `sustained` plus `slope` times the time spent
    /// waiting so far (`FoundationDB`'s `onFailedFor`).
    ///
    /// The slope lets a long wait tolerate proportionally longer outages;
    /// it is clamped to `0.0..=0.99`. While the future is alive the
    /// runtime keeps probing a failing address so the verdict can change.
    /// "Failed for a while" is an observation, never proof that the remote
    /// process is dead.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn on_failed_for(
        &self,
        endpoint: Endpoint,
        sustained: Duration,
        slope: f64,
    ) -> impl Future<Output = Result<(), RpcError>> + Send + 'static {
        let slope = if slope.is_finite() {
            slope.clamp(0.0, 0.99)
        } else {
            0.0
        };
        let monitor = self.clone();
        let reference = PeerReference::new(&self.shared, endpoint.address());
        async move {
            let _reference = reference;
            let start = monitor.time.now();
            loop {
                monitor.on_failed(endpoint).await?;
                if monitor.endpoint_state(&endpoint).is_permanent() {
                    return Ok(());
                }
                // X = sustained + slope * (elapsed + X)
                let elapsed = monitor.time.now().saturating_sub(start);
                let wait =
                    (sustained.as_secs_f64() + slope * elapsed.as_secs_f64()) / (1.0 - slope);
                let wait = Duration::try_from_secs_f64(wait)
                    .unwrap_or(LONGEST_WAIT)
                    .min(LONGEST_WAIT);
                let recovered = monitor.on_available(endpoint.address());
                match monitor.time.timeout(wait, recovered).await {
                    // Recovered in time (or shut down): keep waiting.
                    Ok(result) => result?,
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// Keeps an address probed while it is failing, for as long as it lives.
pub(crate) struct PeerReference<P: Providers> {
    shared: Weak<Shared<P>>,
    address: SocketAddr,
}

impl<P: Providers> PeerReference<P> {
    pub(crate) fn new(shared: &Weak<Shared<P>>, address: SocketAddr) -> Self {
        if let Some(shared) = shared.upgrade() {
            shared.reference(address, true);
        }
        Self {
            shared: shared.clone(),
            address,
        }
    }
}

impl<P: Providers> Drop for PeerReference<P> {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.reference(self.address, false);
        }
    }
}
