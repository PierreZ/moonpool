//! The typed calling and serving surface: references, clients, receivers
//! and reply handles.

pub(crate) mod bootstrap;
pub(crate) mod client;
pub(crate) mod receiver;
pub(crate) mod reply;

pub use bootstrap::{BootstrapAddress, BootstrapClient, BootstrapStats, RetryPolicy, WellKnownRef};
pub use client::{ReplyAttempt, ServiceClient};
pub use receiver::{IncomingRequest, RequestStream};
pub use reply::ReplyHandle;
