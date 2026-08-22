//! Client side: h2 channels in the shape tower and tonic expect.
//!
//! [`H2Channel`] is one established connection exposed as a tower
//! [`Service`](tower_service::Service). [`ReconnectingChannel`] wraps that with
//! connection management (connect on demand, deterministic backoff, reconnect
//! after a loss), which is the role `tonic::transport::Channel` plays in a
//! production tonic stack.

mod channel;
mod config;
mod reconnect;
mod state;

pub use channel::{H2Channel, ResponseFuture};
pub use config::ChannelConfig;
pub use reconnect::ReconnectingChannel;
