//! The typed calling and serving surface: references, clients, receivers
//! and reply handles.

pub(crate) mod client;
pub(crate) mod receiver;
pub(crate) mod reply;

pub use client::{ServiceClient, ServiceRef};
pub use receiver::{IncomingRequest, RequestStream};
pub use reply::ReplyHandle;
