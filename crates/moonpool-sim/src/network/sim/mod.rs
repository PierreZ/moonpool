//! The simulated network engine and its provider types.
//!
//! [`SimNetworkProvider`] implements [`NetworkProvider`](crate::NetworkProvider)
//! and hands out [`SimTcpListener`](crate::network::sim::SimTcpListener)s and
//! [`SimTcpStream`](crate::network::sim::SimTcpStream)s. The engine that
//! owns listeners, connections, topology, faults and wakers is internal: it
//! receives targeted [`NetworkEvent`](crate::NetworkEvent)s from the [`SimWorld`](crate::SimWorld)
//! scheduler and returns its wakes to the coordinator, which runs them after
//! it releases the world lock.

mod delay;
mod delivery;
mod engine;
mod event;
mod facade;
mod faults;
mod provider;
mod state;
mod stream;
mod types;

pub(crate) use delay::NetworkDelay;
pub(crate) use engine::{
    AcceptWaiterId, ConnectWaiterId, NetworkActions, NetworkSimulation, PendingPublish,
};
pub use event::{NetworkEvent, NetworkOperationId};
pub use provider::SimNetworkProvider;
pub use state::CloseReason;
pub use stream::{SimTcpListener, SimTcpStream};
pub use types::{ConnectionId, ListenerId};
