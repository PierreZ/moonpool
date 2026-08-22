//! Simulated networking implementation.

mod provider;
mod stream;
mod types;

pub use provider::SimNetworkProvider;
pub use stream::{SimTcpListener, SimTcpStream};
pub use types::{ConnectionId, ListenerId};
