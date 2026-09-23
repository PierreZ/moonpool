//! The weak, cloneable handle applications register and call through.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use moonpool_core::Providers;

use super::Shared;
use crate::call::client::ServiceClient;
use crate::call::receiver::RequestStream;
use crate::endpoint::{AccessClass, Incarnation, WellKnownId};
use crate::error::{ErrorReason, RpcError};
use crate::failure::FailureMonitor;
use crate::interface::{RpcInterface, ServiceGroup, ServiceRef};
use crate::protocol::RpcMethod;
use crate::stats::{ResourceProbe, RpcStats};

/// A weak, cloneable handle to a running RPC runtime.
///
/// Registers endpoints and binds clients. It never keeps the runtime alive:
/// once the [`RpcDriver`](crate::RpcDriver) is dropped every operation
/// fails with [`ErrorReason::Shutdown`] (or returns `None`).
pub struct RpcHandle<P: Providers> {
    shared: Weak<Shared<P>>,
}

impl<P: Providers> Clone for RpcHandle<P> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<P: Providers> std::fmt::Debug for RpcHandle<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcHandle")
            .field("running", &self.is_running())
            .finish()
    }
}

impl<P: Providers> RpcHandle<P> {
    pub(crate) fn new(shared: &Arc<Shared<P>>) -> Self {
        Self {
            shared: Arc::downgrade(shared),
        }
    }

    pub(crate) fn upgrade(&self) -> Option<Arc<Shared<P>>> {
        self.shared.upgrade()
    }

    fn running(&self) -> Result<Arc<Shared<P>>, RpcError> {
        self.upgrade()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))
    }

    /// Register a new dynamic endpoint for `M` with the given access class.
    ///
    /// Returns the serialisable reference to hand to callers and the owned
    /// receiver. Dropping the receiver destroys the endpoint for good: its
    /// reference never serves again, in this incarnation or any other.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::NotListening`] on a client-only runtime,
    /// [`ErrorReason::Overloaded`] when the endpoint budget is exhausted and
    /// [`ErrorReason::Shutdown`] once the driver is gone.
    pub fn register<M: RpcMethod>(
        &self,
        access: AccessClass,
    ) -> Result<(ServiceRef<M>, RequestStream<M>), RpcError> {
        self.running()?.register::<M>(access, None)
    }

    /// Register a dynamic endpoint group serving interface `I`.
    ///
    /// The group is one registry slot in this incarnation; open each of
    /// its methods with [`ServiceGroup::serve`] and publish
    /// [`ServiceGroup::interface_ref`]. A request for a method the group
    /// does not serve is refused with [`ErrorReason::MethodNotFound`].
    /// Dropping the group destroys it for good, like dropping a
    /// [`register`](Self::register) receiver.
    ///
    /// # Errors
    ///
    /// As for [`register`](Self::register).
    pub fn register_group<I: RpcInterface>(
        &self,
        access: AccessClass,
    ) -> Result<ServiceGroup<I>, RpcError> {
        self.running()?.register_group::<I>(access)
    }

    /// Register `M` at the well-known id `id`.
    ///
    /// A well-known endpoint answers at the same token in every incarnation
    /// of this runtime's address: a caller holding its reference (or a
    /// [`WellKnownRef`](crate::WellKnownRef)) reaches whichever incarnation
    /// currently registered it, so it survives restarts. Use it only for
    /// bootstrap services; everything discovered through them should be
    /// dynamic.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::AlreadyRegistered`] while another receiver holds `id`,
    /// [`ErrorReason::NotListening`] on a client-only runtime and
    /// [`ErrorReason::Shutdown`] once the driver is gone.
    pub fn register_well_known<M: RpcMethod>(
        &self,
        id: WellKnownId,
        access: AccessClass,
    ) -> Result<(ServiceRef<M>, RequestStream<M>), RpcError> {
        self.running()?.register::<M>(access, Some(id))
    }

    /// Bind a reference to this runtime (same as [`ServiceRef::bind`]).
    #[must_use]
    pub fn client<M: RpcMethod>(&self, target: &ServiceRef<M>) -> ServiceClient<P, M> {
        target.bind(self)
    }

    /// This runtime's failure monitor, while it runs.
    #[must_use]
    pub fn failure_monitor(&self) -> Option<FailureMonitor<P>> {
        self.upgrade().map(|shared| FailureMonitor::new(&shared))
    }

    /// Whether the driver still exists.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.shared.strong_count() > 0
    }

    /// This runtime's incarnation, while it runs.
    #[must_use]
    pub fn incarnation(&self) -> Option<Incarnation> {
        self.upgrade().map(|shared| shared.incarnation)
    }

    /// The address advertised in this runtime's endpoints, if listening.
    #[must_use]
    pub fn address(&self) -> Option<SocketAddr> {
        self.upgrade().and_then(|shared| shared.address)
    }

    /// A snapshot of the counters, while the runtime runs.
    #[must_use]
    pub fn stats(&self) -> Option<RpcStats> {
        self.upgrade().map(|shared| shared.stats())
    }

    /// A drop probe that keeps answering after the runtime is gone.
    #[must_use]
    pub fn probe(&self) -> Option<ResourceProbe> {
        self.upgrade().map(|shared| {
            ResourceProbe::new(
                Arc::clone(&shared.counters),
                Arc::downgrade(&shared.alive),
                Arc::downgrade(shared.watch()),
            )
        })
    }
}
