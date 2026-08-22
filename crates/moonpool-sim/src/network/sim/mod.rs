//! Simulated networking implementation.

mod delay;
mod engine;
mod event;
mod facade;
mod provider;
mod state;
mod stream;
mod types;

pub(crate) use delay::NetworkDelay;
pub(crate) use engine::{AcceptWaiterId, NetworkActions, NetworkSimulation};
pub use event::{NetworkEvent, NetworkOperationId};
pub use provider::SimNetworkProvider;
pub use state::CloseReason;
pub use stream::{SimTcpListener, SimTcpStream};
pub use types::{ConnectionId, ListenerId};
