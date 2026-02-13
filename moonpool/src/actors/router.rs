//! Actor router: caller-side resolution and request dispatch.
//!
//! The `ActorRouter` resolves `ActorId → Endpoint` using the directory
//! and placement strategy, then sends the request via `send_request()`
//! to the actor type's dispatch token (index 0).
//!
//! # Flow
//!
//! 1. Directory lookup for the target actor
//! 2. If not found, placement strategy chooses the node
//! 3. Register the new placement in the directory
//! 4. Build `ActorMessage` with target identity, method, and serialized body
//! 5. `send_request()` to `UID::new(actor_type, 0)` on the target node
//! 6. Await `ActorResponse`, deserialize the inner body
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' OutsideRuntimeClient / GrainReference:
//! the caller-side proxy that resolves grain location and sends messages.

use std::rc::Rc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Endpoint, JsonCodec, MessageCodec, NetTransport, Providers, UID, send_request};

use super::{
    ActorDirectory, ActorId, ActorMessage, ActorResponse, DirectoryError, PlacementError,
    PlacementStrategy,
};

/// Errors from actor router operations.
#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    /// Directory lookup or registration failed.
    #[error("directory error: {0}")]
    Directory(#[from] DirectoryError),

    /// Placement strategy failed.
    #[error("placement error: {0}")]
    Placement(#[from] PlacementError),

    /// Transport-level messaging error.
    #[error("messaging error: {0}")]
    Messaging(#[from] crate::error::MessagingError),

    /// Reply error from the remote handler.
    #[error("reply error: {0}")]
    Reply(#[from] crate::ReplyError),

    /// Codec serialization/deserialization error.
    #[error("codec error: {0}")]
    Codec(#[from] crate::CodecError),

    /// The method discriminant is not recognized by the actor handler.
    #[error("unknown method: {0}")]
    UnknownMethod(u32),

    /// The remote actor handler returned an error.
    #[error("handler error: {0}")]
    HandlerError(String),

    /// RPC-level error (wraps MessagingError or ReplyError).
    #[error("rpc error: {0}")]
    Rpc(#[from] crate::RpcError),
}

/// Caller-side actor request router.
///
/// Resolves actor locations via the directory, places new actors via the
/// placement strategy, and dispatches requests using the existing transport.
///
/// # Example
///
/// ```rust,ignore
/// let router = ActorRouter::new(transport, directory, placement, JsonCodec);
///
/// let resp: BalanceResponse = router.send_actor_request(
///     &ActorId { actor_type: BANK_ACCOUNT_TYPE, identity: "alice".into() },
///     bank_methods::DEPOSIT,
///     &DepositRequest { amount: 100 },
/// ).await?;
/// ```
pub struct ActorRouter<P: Providers, C: MessageCodec = JsonCodec> {
    transport: Rc<NetTransport<P>>,
    directory: Rc<dyn ActorDirectory>,
    placement: Rc<dyn PlacementStrategy>,
    codec: C,
}

impl<P: Providers, C: MessageCodec> ActorRouter<P, C> {
    /// Create a new actor router.
    ///
    /// # Arguments
    ///
    /// * `transport` - The transport for sending messages
    /// * `directory` - Directory for resolving actor locations
    /// * `placement` - Strategy for placing new actors
    /// * `codec` - Codec for serializing request/response bodies
    pub fn new(
        transport: Rc<NetTransport<P>>,
        directory: Rc<dyn ActorDirectory>,
        placement: Rc<dyn PlacementStrategy>,
        codec: C,
    ) -> Self {
        Self {
            transport,
            directory,
            placement,
            codec,
        }
    }

    /// Get a reference to the codec used by this router.
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Send a request to a virtual actor.
    ///
    /// Resolves the actor's location (placing if needed), serializes the
    /// method body into an `ActorMessage`, sends it via the transport to
    /// token 0 of the actor's type, and deserializes the response.
    ///
    /// # Arguments
    ///
    /// * `target` - The target actor identity
    /// * `method` - Method discriminant (1, 2, 3, …)
    /// * `req` - The method-specific request payload
    ///
    /// # Type Parameters
    ///
    /// * `Req` - Request type (must be serializable)
    /// * `Resp` - Response type (must be deserializable)
    pub async fn send_actor_request<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        target: &ActorId,
        method: u32,
        req: &Req,
    ) -> Result<Resp, ActorError> {
        // 1. Resolve actor location
        let endpoint = self.resolve(target).await?;

        // 2. Serialize the method body
        let body = self.codec.encode(req).map_err(ActorError::Codec)?;

        // 3. Build ActorMessage
        let actor_msg = ActorMessage {
            target: target.clone(),
            sender: None,
            method,
            body,
            forward_count: 0,
        };

        // 4. Send via transport to the actor type's dispatch token (index 0)
        let dest = Endpoint::new(endpoint.address.clone(), UID::new(target.actor_type.0, 0));
        let future = send_request(&self.transport, &dest, actor_msg, self.codec.clone())?;

        // 5. Await and decode response
        let response: ActorResponse = future.await?;

        // 6. Handle cache invalidation if present
        if let Some(invalidation) = &response.cache_invalidation {
            // Update our directory cache with the correct endpoint
            let _ = self.directory.unregister(&invalidation.actor_id).await;
            if let Some(valid) = &invalidation.valid_endpoint {
                let _ = self
                    .directory
                    .register(&invalidation.actor_id, valid.clone())
                    .await;
            }
        }

        let body = response.body.map_err(ActorError::HandlerError)?;
        let result = self.codec.decode(&body).map_err(ActorError::Codec)?;
        Ok(result)
    }

    /// Resolve an actor's endpoint, placing it if not found in the directory.
    async fn resolve(&self, target: &ActorId) -> Result<Endpoint, ActorError> {
        // Check directory first
        if let Some(endpoint) = self.directory.lookup(target).await? {
            return Ok(endpoint);
        }

        // Not found — place the actor
        let endpoint = self.placement.place(target, &[]).await?;

        // Register in directory (ignore AlreadyRegistered race — another caller
        // may have placed the same actor concurrently)
        match self.directory.register(target, endpoint.clone()).await {
            Ok(()) => {}
            Err(DirectoryError::AlreadyRegistered { .. }) => {
                // Another caller placed it first — use their placement
                if let Some(ep) = self.directory.lookup(target).await? {
                    return Ok(ep);
                }
            }
            Err(e) => return Err(e.into()),
        }

        Ok(endpoint)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::{JsonCodec, NetworkAddress, UID};

    use super::*;
    use crate::actors::{ActorType, InMemoryDirectory, LocalPlacement};

    // We use the same MockProviders pattern as transport tests
    #[derive(Clone)]
    struct MockNetworkProvider;

    struct DummyStream;

    impl tokio::io::AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }
    }

    impl tokio::io::AsyncWrite for DummyStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }
    }

    impl std::marker::Unpin for DummyStream {}

    struct DummyListener;

    #[async_trait::async_trait(?Send)]
    impl crate::TcpListenerTrait for DummyListener {
        type TcpStream = DummyStream;

        async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
            Err(std::io::Error::other("dummy"))
        }

        fn local_addr(&self) -> std::io::Result<String> {
            Err(std::io::Error::other("dummy"))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl crate::NetworkProvider for MockNetworkProvider {
        type TcpStream = DummyStream;
        type TcpListener = DummyListener;

        async fn bind(&self, _addr: &str) -> std::io::Result<Self::TcpListener> {
            Err(std::io::Error::other("mock"))
        }

        async fn connect(&self, _addr: &str) -> std::io::Result<Self::TcpStream> {
            Err(std::io::Error::other("mock"))
        }
    }

    #[derive(Clone)]
    struct MockProviders {
        network: MockNetworkProvider,
        time: crate::TokioTimeProvider,
        task: crate::TokioTaskProvider,
        random: crate::TokioRandomProvider,
        storage: crate::TokioStorageProvider,
    }

    impl MockProviders {
        fn new() -> Self {
            Self {
                network: MockNetworkProvider,
                time: crate::TokioTimeProvider::new(),
                task: crate::TokioTaskProvider,
                random: crate::TokioRandomProvider::new(),
                storage: crate::TokioStorageProvider::new(),
            }
        }
    }

    impl Providers for MockProviders {
        type Network = MockNetworkProvider;
        type Time = crate::TokioTimeProvider;
        type Task = crate::TokioTaskProvider;
        type Random = crate::TokioRandomProvider;
        type Storage = crate::TokioStorageProvider;

        fn network(&self) -> &Self::Network {
            &self.network
        }
        fn time(&self) -> &Self::Time {
            &self.time
        }
        fn task(&self) -> &Self::Task {
            &self.task
        }
        fn random(&self) -> &Self::Random {
            &self.random
        }
        fn storage(&self) -> &Self::Storage {
            &self.storage
        }
    }

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500)
    }

    #[test]
    fn test_router_creation() {
        let transport = crate::NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        let local_endpoint = Endpoint::new(test_address(), UID::new(0xBA4E_4B00, 0));
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));

        let _router = ActorRouter::new(transport, directory, placement, JsonCodec);
    }

    #[tokio::test]
    async fn test_resolve_places_unknown_actor() {
        let transport = crate::NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        let local_endpoint = Endpoint::new(test_address(), UID::new(0xBA4E_4B00, 0));
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let placement: Rc<dyn PlacementStrategy> =
            Rc::new(LocalPlacement::new(local_endpoint.clone()));

        let router = ActorRouter::new(transport, directory.clone(), placement, JsonCodec);

        let actor_id = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        };

        // Resolve should place the actor and register it
        let resolved = router
            .resolve(&actor_id)
            .await
            .expect("resolve should succeed");
        assert_eq!(resolved, local_endpoint);

        // Directory should now have the entry
        let lookup = directory
            .lookup(&actor_id)
            .await
            .expect("lookup should succeed");
        assert_eq!(lookup, Some(local_endpoint));
    }

    #[tokio::test]
    async fn test_resolve_uses_directory_entry() {
        let transport = crate::NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        let local_endpoint = Endpoint::new(test_address(), UID::new(0xBA4E_4B00, 0));
        let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
        let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));

        let router = ActorRouter::new(transport, directory.clone(), placement, JsonCodec);

        let actor_id = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "bob".to_string(),
        };

        // Pre-register in directory
        let remote_endpoint = Endpoint::new(
            NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4501),
            UID::new(0xBA4E_4B00, 0),
        );
        directory
            .register(&actor_id, remote_endpoint.clone())
            .await
            .expect("register should succeed");

        // Resolve should find the existing entry
        let resolved = router
            .resolve(&actor_id)
            .await
            .expect("resolve should succeed");
        assert_eq!(resolved, remote_endpoint);
    }
}
