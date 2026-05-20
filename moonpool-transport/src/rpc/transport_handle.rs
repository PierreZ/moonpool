//! `TransportHandle`: Object-safe trait erasing provider and codec generics.
//!
//! User-facing types (`ServiceEndpoint`, `RequestStream`, etc.) hold
//! `Arc<dyn TransportHandle>` instead of `Arc<NetTransport<P>>`, which erases
//! the `P: Providers` generic from their signatures. The codec generic `C`
//! is erased separately via typed closures ([`EncodeFn`] / [`DecodeFn`]).

use std::sync::{Arc, Weak};

use crate::error::MessagingError;
use crate::rpc::endpoint_map::MessageReceiver;
use crate::rpc::failure_monitor::FailureMonitor;
use crate::rpc::net_notified_queue::ReplyQueueCloser;
use crate::{CodecError, Endpoint, MessageCodec, NetworkAddress, UID};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Type-erased encode function captured from a concrete [`MessageCodec`].
pub type EncodeFn<T> = Arc<dyn Fn(&T) -> Result<Vec<u8>, CodecError> + Send + Sync>;

/// Type-erased decode function captured from a concrete [`MessageCodec`].
pub type DecodeFn<T> = Arc<dyn Fn(&[u8]) -> Result<T, CodecError> + Send + Sync>;

/// Create an [`EncodeFn`] by capturing a concrete codec.
pub fn make_encode_fn<T: Serialize + Send + Sync + 'static, C: MessageCodec>(
    codec: C,
) -> EncodeFn<T> {
    Arc::new(move |val: &T| codec.encode(val))
}

/// Create a [`DecodeFn`] by capturing a concrete codec.
pub fn make_decode_fn<T: DeserializeOwned + Send + Sync + 'static, C: MessageCodec>(
    codec: C,
) -> DecodeFn<T> {
    Arc::new(move |buf: &[u8]| codec.decode::<T>(buf))
}

/// Object-safe transport abstraction that erases provider generics.
///
/// `NetTransport<P>` implements this trait, allowing user-facing types
/// to hold `Arc<dyn TransportHandle>` without naming `P`.
pub trait TransportHandle: Send + Sync {
    /// Send payload unreliably (best-effort, dropped on failure).
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError`] if the endpoint is unknown, the peer rejects
    /// the packet, or transport-level send fails.
    fn send_unreliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), MessagingError>;

    /// Send payload reliably (queued, retried on reconnect).
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError`] if the endpoint is unknown, the peer rejects
    /// the packet, or transport-level send fails.
    fn send_reliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), MessagingError>;

    /// Register a dynamic endpoint receiver, returning the endpoint.
    fn register(&self, token: UID, receiver: Arc<dyn MessageReceiver>) -> Endpoint;

    /// Unregister a dynamic endpoint.
    fn unregister(&self, token: &UID) -> Option<Arc<dyn MessageReceiver>>;

    /// Track a pending reply queue for disconnect cleanup.
    fn register_pending_reply(&self, addr: &str, token: UID, closer: Weak<dyn ReplyQueueCloser>);

    /// Get the failure monitor.
    fn failure_monitor(&self) -> Arc<FailureMonitor>;

    /// Get the local address of this transport.
    fn local_address(&self) -> &NetworkAddress;

    /// Allocate a random base token for a dynamic interface.
    fn allocate_interface_token(&self) -> UID;

    /// Generate a random UID (for reply endpoints, etc.).
    fn random_uid(&self) -> UID;

    /// Check if an address is local to this transport.
    fn is_local_address(&self, address: &NetworkAddress) -> bool;

    /// Get a weak reference for drop cleanup callbacks.
    fn weak_for_cleanup(&self) -> Weak<dyn TransportHandle>;
}
