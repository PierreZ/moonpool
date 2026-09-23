//! Endpoint groups: one registry slot serving several methods.

use std::marker::PhantomData;
use std::sync::Weak;

use super::{InterfaceMethod, InterfaceRef, RpcInterface, ServiceRef};
use crate::call::receiver::{EndpointOwner, RequestStream, endpoint_pair};
use crate::endpoint::{AccessClass, Endpoint};
use crate::error::{ErrorReason, RpcError};

/// The owned server side of a dynamic endpoint group serving interface
/// `I`, from [`RpcHandle::register_group`](crate::RpcHandle::register_group).
///
/// Open each method with [`serve`](Self::serve) and hand out
/// [`interface_ref`](Self::interface_ref) (or one method's
/// [`service_ref`](Self::service_ref)). Dropping the group destroys it; see
/// the [module docs](crate::interface) for what happens to queued and
/// admitted requests.
#[must_use = "dropping a group destroys its endpoint"]
pub struct ServiceGroup<I: RpcInterface> {
    endpoint: Endpoint,
    access: AccessClass,
    owner: Weak<dyn EndpointOwner>,
    _interface: PhantomData<fn() -> I>,
}

impl<I: RpcInterface> ServiceGroup<I> {
    pub(crate) fn new(
        endpoint: Endpoint,
        access: AccessClass,
        owner: Weak<dyn EndpointOwner>,
    ) -> Self {
        Self {
            endpoint,
            access,
            owner,
            _interface: PhantomData,
        }
    }

    /// The group's endpoint.
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// The serialisable reference to the whole group.
    #[must_use]
    pub fn interface_ref(&self) -> InterfaceRef<I> {
        InterfaceRef::new(self.endpoint, self.access)
    }

    /// The serialisable reference to one method of the group.
    #[must_use]
    pub fn service_ref<M: InterfaceMethod<I>>(&self) -> ServiceRef<M> {
        ServiceRef::in_interface::<I>(self.endpoint, self.access)
    }

    /// Start serving method `M`: its requests are admitted from now on and
    /// arrive on the returned stream.
    ///
    /// # Errors
    ///
    /// [`ErrorReason::AlreadyRegistered`] while another stream serves `M`
    /// in this group, [`ErrorReason::Shutdown`] once the runtime is gone.
    pub fn serve<M: InterfaceMethod<I>>(&self) -> Result<RequestStream<M>, RpcError> {
        let owner = self
            .owner
            .upgrade()
            .ok_or(RpcError::not_admitted(ErrorReason::Shutdown))?;
        let (inbox, receiver) = endpoint_pair::<M>(owner.queue_capacity());
        owner
            .attach(self.endpoint.token(), M::METHOD, inbox)
            .map_err(RpcError::not_admitted)?;
        tracing::debug!(interface = I::NAME, method = M::NAME, endpoint = %self.endpoint, "rpc group method served");
        Ok(receiver.bind(self.service_ref(), self.owner.clone(), true))
    }
}

impl<I: RpcInterface> Drop for ServiceGroup<I> {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.unregister(self.endpoint.token(), None);
        }
    }
}

impl<I: RpcInterface> std::fmt::Debug for ServiceGroup<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceGroup")
            .field("interface", &I::NAME)
            .field("endpoint", &self.endpoint)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}
