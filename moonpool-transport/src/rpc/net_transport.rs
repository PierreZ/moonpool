//! `NetTransport`: Central transport coordinator (FDB pattern).
//!
//! Manages peer connections and dispatches incoming packets to endpoints.
//! Provides synchronous send API (FDB pattern: never await on send).
//!
//! # FDB Reference
//! From NetTransport.actor.cpp:300-600, NetTransport.h:195-314
//!
//! # Usage
//!
//! Use [`NetTransportBuilder`] to create a properly configured transport:
//!
//! ```rust,ignore
//! // For servers and clients that need RPC responses:
//! let transport = NetTransportBuilder::new(network, time, task)
//!     .local_address(addr)
//!     .build_listening()
//!     .await?;
//!
//! // For fire-and-forget senders only (no listening):
//! let transport = NetTransportBuilder::new(network, time, task)
//!     .local_address(addr)
//!     .build();
//! ```
//!
//! The builder automatically handles `Arc` wrapping and `set_weak_self()`,
//! eliminating the most common footgun in `NetTransport` usage.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, Weak};

use super::failure_monitor::FailureMonitor;
use super::net_notified_queue::ReplyQueueCloser;
use super::reply_error::ReplyError;
use crate::{
    Endpoint, NetworkAddress, NetworkProvider, Peer, PeerConfig, Providers, TaskProvider,
    TcpListenerTrait, UID, WellKnownToken,
};
use tokio::sync::watch;

use super::endpoint_map::{EndpointMap, MessageReceiver};
use super::request_stream::RequestStream;
use crate::MessageCodec;
use crate::error::MessagingError;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Type alias for shared peer reference.
type SharedPeer<P> = Arc<RwLock<Peer<P>>>;

/// Pending reply entry: token + weak reference to the queue closer.
type PendingReplyEntry = (UID, Weak<dyn ReplyQueueCloser>);

/// Internal transport data (FDB `TransportData` equivalent).
///
/// Separates internal mutable state from public API, matching FDB's pattern.
/// See NetTransport.actor.cpp:300-350 for the original `TransportData` struct.
///
/// # FDB Reference
/// ```cpp
/// struct TransportData {
///     std::unordered_map<NetworkAddress, Reference<struct Peer>> peers;
///     std::unordered_map<NetworkAddress, std::pair<double, double>> closedPeers;
///     // ... endpoints, stats, etc.
/// };
/// ```
struct TransportData<P: Providers> {
    /// Endpoint map for routing incoming packets.
    endpoints: EndpointMap,

    /// Peer connections keyed by destination address (outgoing).
    /// FDB: `std::unordered_map`<`NetworkAddress`, Reference<struct Peer>> peers;
    peers: BTreeMap<String, SharedPeer<P>>,

    /// Incoming peer connections (from accepted connections).
    /// Separate from outgoing peers to avoid conflicts.
    incoming_peers: BTreeMap<String, SharedPeer<P>>,

    /// Failure monitor for address/endpoint failure tracking.
    /// FDB: `IFailureMonitor` (FailureMonitor.h)
    failure_monitor: Arc<FailureMonitor>,

    /// Pending reply queues per remote address.
    ///
    /// Uses Weak refs so entries are automatically invalidated when `ReplyFuture` drops.
    /// Cleaned lazily during `close_pending_replies`.
    ///
    /// FDB: endStreamOnDisconnect pattern (genericactors.actor.h:332)
    pending_replies: BTreeMap<String, Vec<PendingReplyEntry>>,

    /// Statistics.
    stats: TransportStats,
}

impl<P: Providers> TransportData<P> {
    fn new(providers: &P) -> Self {
        Self {
            endpoints: EndpointMap::new(),
            peers: BTreeMap::new(),
            incoming_peers: BTreeMap::new(),
            failure_monitor: Arc::new(FailureMonitor::new(providers.time().clone())),
            pending_replies: BTreeMap::new(),
            stats: TransportStats::default(),
        }
    }
}

/// Central transport coordinator (FDB `NetTransport` equivalent).
///
/// # Design
///
/// - Manages peer connections lazily (created on first send)
/// - Routes incoming packets to registered endpoints
/// - Synchronous send API - queues immediately, returns (FDB pattern)
/// - Explicit `Arc<NetTransport>` passing (testable, simulation-friendly)
/// - Internal state in `TransportData` (matches FDB: `TransportData* self`)
///
/// # FDB Reference
/// From NetTransport.h:195-314
///
/// # Multi-Node Support (Phase 12 Step 7d)
///
/// For multi-node operation, wrap in `Arc` and call `set_weak_self()`:
/// ```ignore
/// let transport = Arc::new(NetTransport::new(...));
/// transport.set_weak_self(Arc::downgrade(&transport));
/// transport.listen().await?; // Start accepting connections
/// ```
pub struct NetTransport<P: Providers, C: MessageCodec = crate::JsonCodec> {
    /// Internal transport data (FDB: `TransportData`* self).
    data: RwLock<TransportData<P>>,

    /// Local address for this transport.
    local_address: NetworkAddress,

    /// Providers bundle for network, time, task, and random.
    providers: P,

    /// Message codec for serialization/deserialization.
    codec: C,

    /// Peer configuration.
    peer_config: PeerConfig,

    /// Weak self-reference for spawning background tasks.
    /// Required for `connection_reader` tasks to dispatch back to this transport.
    /// Set via `set_weak_self()` after wrapping in `Arc`.
    weak_self: RwLock<Option<Weak<Self>>>,

    /// Shutdown signal sender. When set to `true`, background tasks (`listen_task`) should exit.
    /// Uses watch channel so multiple receivers can observe the same signal.
    shutdown_tx: watch::Sender<bool>,

    /// Weak reference as `dyn TransportHandle` for codec-erased types.
    /// Set by the builder alongside `weak_self`.
    weak_handle: RwLock<Option<std::sync::Weak<dyn super::transport_handle::TransportHandle>>>,
}

/// Statistics for the transport.
#[derive(Debug, Default)]
struct TransportStats {
    /// Number of packets sent.
    packets_sent: u64,
    /// Number of packets dispatched to endpoints.
    packets_dispatched: u64,
    /// Number of packets that couldn't be delivered (endpoint not found).
    packets_undelivered: u64,
    /// Number of peers created.
    peers_created: u64,
}

impl<P: Providers + Send + Sync, C: MessageCodec> NetTransport<P, C> {
    /// Create a new `NetTransport`.
    ///
    /// # Arguments
    ///
    /// * `local_address` - Address of this transport (for local delivery checks)
    /// * `providers` - Providers bundle for network, time, task, and random
    /// * `codec` - Message codec for serialization/deserialization
    ///
    /// # Multi-Node Usage
    ///
    /// For multi-node operation, wrap in `Arc` and call `set_weak_self()`:
    /// ```ignore
    /// let transport = Arc::new(NetTransport::new(...));
    /// transport.set_weak_self(Arc::downgrade(&transport));
    /// ```
    pub fn new(local_address: NetworkAddress, providers: P, codec: C) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        let data = TransportData::new(&providers);
        Self {
            data: RwLock::new(data),
            local_address,
            providers,
            codec,
            peer_config: PeerConfig::default(),
            weak_self: RwLock::new(None),
            shutdown_tx,
            weak_handle: RwLock::new(None),
        }
    }

    /// Get a reference to the transport's codec.
    pub fn codec(&self) -> &C {
        &self.codec
    }

    /// Get the random provider for generating unique IDs.
    pub fn random(&self) -> &P::Random {
        self.providers.random()
    }

    /// Get the providers bundle.
    pub fn providers(&self) -> &P {
        &self.providers
    }

    /// Allocate a random base token for a dynamic interface.
    ///
    /// Each method in the interface uses `base.adjusted(method_index)`.
    /// Method indices start at 1 (0 is the base identity).
    pub fn allocate_interface_token(&self) -> UID {
        use crate::RandomProvider as _;
        UID::new(
            self.providers.random().random::<u64>(),
            self.providers.random().random::<u64>(),
        )
    }

    /// Set the weak self-reference for background task spawning.
    ///
    /// Required for multi-node operation where `connection_reader` tasks need
    /// to dispatch incoming packets back to this transport.
    ///
    /// # FDB Pattern
    /// Similar to how FDB actors reference `TransportData* self`.
    ///
    /// # Example
    /// ```ignore
    /// let transport = Arc::new(NetTransport::new(...));
    /// transport.set_weak_self(Arc::downgrade(&transport));
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn set_weak_self(&self, weak: Weak<Self>) {
        *self
            .weak_self
            .write()
            .expect("RwLock poisoned: prior task panicked") = Some(weak);
    }

    /// Get weak self-reference, panics if not set.
    fn weak_self(&self) -> Weak<Self> {
        self.weak_self
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .clone()
            .expect("weak_self not set - call set_weak_self() after wrapping in Arc")
    }

    /// Create with custom peer configuration.
    #[must_use]
    pub fn with_peer_config(mut self, config: PeerConfig) -> Self {
        self.peer_config = config;
        self
    }

    /// Get the local address.
    pub fn local_address(&self) -> &NetworkAddress {
        &self.local_address
    }

    /// Get the failure monitor for this transport.
    ///
    /// Used by delivery mode functions to race replies against disconnect signals.
    ///
    /// # FDB Reference
    /// `IFailureMonitor::failureMonitor()` (FailureMonitor.h)
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn failure_monitor(&self) -> Arc<FailureMonitor> {
        Arc::clone(
            &self
                .data
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .failure_monitor,
        )
    }

    /// Register a well-known endpoint.
    ///
    /// Well-known endpoints have deterministic tokens for O(1) lookup.
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError`] if a receiver is already registered under `token`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn register_well_known(
        &self,
        token: WellKnownToken,
        receiver: Arc<dyn MessageReceiver>,
    ) -> Result<(), MessagingError> {
        self.data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .endpoints
            .insert_well_known(token, receiver)
    }

    /// Register a dynamic endpoint with the given UID.
    ///
    /// Returns the endpoint that senders should use to address this receiver.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn register(&self, token: UID, receiver: Arc<dyn MessageReceiver>) -> Endpoint {
        self.data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .endpoints
            .insert(token, receiver);
        Endpoint::new(self.local_address.clone(), token)
    }

    /// Unregister a dynamic endpoint.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn unregister(&self, token: &UID) -> Option<Arc<dyn MessageReceiver>> {
        self.data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .endpoints
            .remove(token)
    }

    /// Register a pending reply queue for a remote address.
    ///
    /// Called by `send_request` to track which reply queues should be closed
    /// when a peer disconnects. Uses Weak refs so stale entries are cleaned lazily.
    pub(crate) fn register_pending_reply(
        &self,
        addr: &str,
        token: UID,
        closer: Weak<dyn ReplyQueueCloser>,
    ) {
        let mut data = self
            .data
            .write()
            .expect("RwLock poisoned: prior task panicked");
        data.pending_replies
            .entry(addr.to_string())
            .or_default()
            .push((token, closer));
    }

    /// Close all pending reply queues for a disconnected address.
    ///
    /// Upgrades Weak refs and calls `close_with_error`, then removes the
    /// corresponding endpoints from the `EndpointMap`.
    ///
    /// FDB: endStreamOnDisconnect pattern (genericactors.actor.h:332)
    pub(crate) fn close_pending_replies(&self, addr: &str, reason: &ReplyError) {
        let entries = {
            let mut data = self
                .data
                .write()
                .expect("RwLock poisoned: prior task panicked");
            data.pending_replies.remove(addr).unwrap_or_default()
        };

        if entries.is_empty() {
            return;
        }

        tracing::debug!(
            "close_pending_replies: closing {} reply queues for {}",
            entries.len(),
            addr,
        );

        for (token, weak_closer) in entries {
            if let Some(closer) = weak_closer.upgrade() {
                closer.close_with_error(reason.clone());
            }
            // Remove from endpoint map regardless (either closed or already dropped)
            self.data
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .endpoints
                .remove(&token);
        }
    }

    /// Register a typed request handler in a single step.
    ///
    /// This is the preferred method for registering RPC handlers. It combines:
    /// - Creating an endpoint from the local address and token
    /// - Creating a `RequestStream` for the request type
    /// - Registering the stream's queue with the transport
    ///
    /// The codec is taken from the transport (set at builder time).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let stream = NetTransport::register_handler::<PingRequest, PingResponse>(
    ///     &transport, ping_token(),
    /// );
    ///
    /// loop {
    ///     if let Some((req, reply)) = stream.recv().await {
    ///         reply.send(PingResponse { pong: req.ping });
    ///     }
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn register_handler<Req, Resp>(
        transport: &Arc<Self>,
        token: UID,
    ) -> RequestStream<Req, Resp>
    where
        Req: DeserializeOwned + Send + Sync + 'static,
        Resp: Serialize + Send + Sync + 'static,
    {
        let endpoint = Endpoint::new(transport.local_address.clone(), token);
        let handle: Arc<dyn super::transport_handle::TransportHandle> =
            Arc::clone(transport) as Arc<dyn super::transport_handle::TransportHandle>;
        let stream = RequestStream::new(endpoint, transport.codec.clone(), handle);
        transport
            .data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .endpoints
            .insert(token, stream.queue() as Arc<dyn MessageReceiver>);
        stream
    }

    /// Register a handler for a multi-method interface.
    ///
    /// The token is computed deterministically from `interface_id` and `method_index`.
    /// The codec is taken from the transport.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (add_stream, _) = NetTransport::register_handler_at::<AddRequest, AddResponse>(
    ///     &transport, CALC_INTERFACE, METHOD_ADD,
    /// );
    ///
    /// loop {
    ///     tokio::select! {
    ///         Some((req, reply)) = add_stream.recv() => {
    ///             reply.send(AddResponse { result: req.a + req.b });
    ///         }
    ///     }
    /// }
    /// ```
    pub fn register_handler_at<Req, Resp>(
        transport: &Arc<Self>,
        interface_id: u64,
        method_index: u64,
    ) -> (RequestStream<Req, Resp>, UID)
    where
        Req: DeserializeOwned + Send + Sync + 'static,
        Resp: Serialize + Send + Sync + 'static,
    {
        let token = UID::new(interface_id, method_index);
        let stream = Self::register_handler(transport, token);
        (stream, token)
    }

    /// Send packet unreliably (best-effort, dropped on failure).
    ///
    /// This is a synchronous operation - it queues the packet and returns immediately.
    /// The packet may be dropped if the connection fails (FDB pattern).
    ///
    /// # Arguments
    ///
    /// * `endpoint` - Destination endpoint
    /// * `payload` - Message bytes (already serialized)
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError`] if local delivery fails or the peer rejects the packet.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn send_unreliable(
        &self,
        endpoint: &Endpoint,
        payload: &[u8],
    ) -> Result<(), MessagingError> {
        // Check for local delivery
        if self.is_local_address(&endpoint.address) {
            return self.deliver_local(&endpoint.token, payload);
        }

        // Get or create peer for remote address
        let peer = self.get_or_open_peer(&endpoint.address);
        peer.write()
            .expect("RwLock poisoned: prior task panicked")
            .send_unreliable(endpoint.token, payload)
            .map_err(|e| MessagingError::PeerError {
                message: e.to_string(),
            })?;

        self.data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .stats
            .packets_sent += 1;
        Ok(())
    }

    /// Send packet reliably (queued, will retry on reconnect).
    ///
    /// This is a synchronous operation - it queues the packet and returns immediately.
    /// The packet will be retried on connection failure (FDB pattern).
    ///
    /// # Arguments
    ///
    /// * `endpoint` - Destination endpoint
    /// * `payload` - Message bytes (already serialized)
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError`] if local delivery fails or the peer rejects the packet.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn send_reliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), MessagingError> {
        // Check for local delivery
        if self.is_local_address(&endpoint.address) {
            return self.deliver_local(&endpoint.token, payload);
        }

        // Get or create peer for remote address
        let peer = self.get_or_open_peer(&endpoint.address);
        peer.write()
            .expect("RwLock poisoned: prior task panicked")
            .send_reliable(endpoint.token, payload)
            .map_err(|e| MessagingError::PeerError {
                message: e.to_string(),
            })?;

        self.data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .stats
            .packets_sent += 1;
        Ok(())
    }

    /// Check if address is local (same as this transport).
    pub(crate) fn is_local_address(&self, address: &NetworkAddress) -> bool {
        self.local_address == *address
    }

    /// Deliver packet locally (same process, no network).
    fn deliver_local(&self, token: &UID, payload: &[u8]) -> Result<(), MessagingError> {
        let data = self
            .data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        if let Some(receiver) = data.endpoints.get(token) {
            receiver.receive(payload);
            drop(data); // Release borrow before mutating stats
            self.data
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .stats
                .packets_dispatched += 1;
            Ok(())
        } else {
            drop(data); // Release borrow before mutating stats
            self.data
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .stats
                .packets_undelivered += 1;
            Err(MessagingError::EndpointNotFound { token: *token })
        }
    }

    /// Get or create a peer for the given address.
    ///
    /// Peers are created lazily on first send (FDB connectionKeeper pattern).
    /// When a new peer is created, a `connection_reader` task is spawned to
    /// handle incoming packets (FDB: connectionKeeper spawns connectionReader at line 843).
    ///
    /// Also checks incoming peers (from accepted connections) as a fallback.
    /// This enables response routing over existing connections — when a server
    /// receives a request from a client, it can send the response back on the
    /// same connection without requiring the client to listen for new connections.
    ///
    /// # FDB Reference
    /// `Reference<struct Peer> getOrOpenPeer(NetworkAddress const& address);`
    fn get_or_open_peer(&self, address: &NetworkAddress) -> SharedPeer<P> {
        let addr_str = address.to_string();

        // Check if outgoing peer already exists
        if let Some(peer) = self
            .data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .peers
            .get(&addr_str)
        {
            return Arc::clone(peer);
        }

        // Check incoming peers — reuse accepted connection for responses
        // (FDB pattern: responses flow back on the same connection)
        if let Some(peer) = self
            .data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .incoming_peers
            .get(&addr_str)
        {
            return Arc::clone(peer);
        }

        // Create new peer with failure monitor
        let fm = Some(Arc::clone(
            &self
                .data
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .failure_monitor,
        ));
        let peer = Peer::new(
            &self.providers,
            addr_str.clone(),
            self.peer_config.clone(),
            fm,
        );
        let peer = Arc::new(RwLock::new(peer));

        // Store in peers map
        {
            let mut data = self
                .data
                .write()
                .expect("RwLock poisoned: prior task panicked");
            data.peers.insert(addr_str.clone(), Arc::clone(&peer));
            data.stats.peers_created += 1;
        }

        // Spawn connection_reader for incoming packets (FDB pattern: connectionKeeper spawns connectionReader)
        // This handles responses for outgoing requests
        self.spawn_connection_reader(Arc::clone(&peer), addr_str);

        peer
    }

    /// Dispatch an incoming packet to the appropriate endpoint.
    ///
    /// Called by the transport loop when a packet is received.
    ///
    /// # Returns
    ///
    /// Ok(()) if delivered, Err if endpoint not found.
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError::EndpointNotFound`] if no receiver is registered
    /// under `token`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn dispatch(&self, token: &UID, payload: &[u8]) -> Result<(), MessagingError> {
        let data = self
            .data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        if let Some(receiver) = data.endpoints.get(token) {
            receiver.receive(payload);
            drop(data); // Release borrow before mutating stats
            self.data
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .stats
                .packets_dispatched += 1;
            Ok(())
        } else {
            drop(data); // Release borrow before mutating stats
            self.data
                .write()
                .expect("RwLock poisoned: prior task panicked")
                .stats
                .packets_undelivered += 1;
            Err(MessagingError::EndpointNotFound { token: *token })
        }
    }

    /// Get statistics.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn packets_sent(&self) -> u64 {
        self.data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .stats
            .packets_sent
    }

    /// Get the number of packets dispatched to endpoints.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn packets_dispatched(&self) -> u64 {
        self.data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .stats
            .packets_dispatched
    }

    /// Get the number of packets that couldn't be delivered.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn packets_undelivered(&self) -> u64 {
        self.data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .stats
            .packets_undelivered
    }

    /// Get the number of peers created.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn peers_created(&self) -> u64 {
        self.data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .stats
            .peers_created
    }

    /// Get number of active peers.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn peer_count(&self) -> usize {
        self.data
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .peers
            .len()
    }

    /// Get number of registered endpoints.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub fn endpoint_count(&self) -> usize {
        let data = self
            .data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        data.endpoints.well_known_count() + data.endpoints.dynamic_count()
    }

    /// Spawn a `connection_reader` for a peer.
    ///
    /// FDB Pattern: connectionKeeper spawns connectionReader (line 843).
    /// The `connection_reader` reads from the peer and dispatches to endpoints.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer to read from
    /// * `peer_addr` - Address string for logging
    fn spawn_connection_reader(&self, peer: SharedPeer<P>, peer_addr: String) {
        // Only spawn if weak_self is set (multi-node mode)
        if self
            .weak_self
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .is_none()
        {
            return;
        }

        let transport_weak = self.weak_self();
        drop(self.providers.task().spawn_task(
            "connection_reader",
            connection_reader(transport_weak, peer, peer_addr),
        ));
    }

    // =========================================================================
    // Server Listener Support (FDB: listen + connectionIncoming)
    // =========================================================================

    /// Start listening for incoming connections.
    ///
    /// FDB Pattern: `listen()` (NetTransport.actor.cpp:1646-1676)
    /// Binds to the local address and spawns an accept loop that handles
    /// incoming connections via `connection_incoming()`.
    ///
    /// # Requirements
    ///
    /// Must call `set_weak_self()` before calling this method.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let transport = Arc::new(NetTransport::new(...));
    /// transport.set_weak_self(Arc::downgrade(&transport));
    /// transport.listen().await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`MessagingError`] if `set_weak_self()` was not called, if binding
    /// the local address fails, or if accepting incoming connections fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (only possible if a prior
    /// task panicked while holding the lock).
    pub async fn listen(&self) -> Result<(), MessagingError> {
        // Verify weak_self is set
        if self
            .weak_self
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .is_none()
        {
            return Err(MessagingError::InvalidState {
                message: "weak_self not set - call set_weak_self() before listen()".to_string(),
            });
        }

        // Bind to local address
        let addr_str = self.local_address.to_string();
        let listener = self
            .providers
            .network()
            .bind(&addr_str)
            .await
            .map_err(|e| MessagingError::NetworkError {
                message: format!("Failed to bind to {addr_str}: {e}"),
            })?;

        tracing::info!("NetTransport: listening on {}", addr_str);

        // Spawn listen task (FDB: listen() actor)
        // Pass shutdown receiver so the task can exit when transport is dropped
        let transport_weak = self.weak_self();
        let shutdown_rx = self.shutdown_tx.subscribe();
        drop(self.providers.task().spawn_task(
            "listen",
            listen_task(transport_weak, listener, addr_str, shutdown_rx),
        ));

        Ok(())
    }
}

/// Implement Drop to signal shutdown to background tasks.
///
/// When `NetTransport` is dropped, we signal all background tasks (`listen_task`)
/// to exit gracefully. This prevents tasks from being stuck on `accept()` forever.
impl<P: Providers, C: MessageCodec> Drop for NetTransport<P, C> {
    fn drop(&mut self) {
        tracing::debug!("NetTransport: signaling shutdown to background tasks");
        // Signal shutdown - ignore error if no receivers
        let _ = self.shutdown_tx.send(true);
    }
}

// =============================================================================
// TransportHandle implementation
// =============================================================================

impl<P: Providers + Send + Sync, C: MessageCodec> super::transport_handle::TransportHandle
    for NetTransport<P, C>
{
    fn send_unreliable(
        &self,
        endpoint: &crate::Endpoint,
        payload: &[u8],
    ) -> Result<(), crate::error::MessagingError> {
        NetTransport::send_unreliable(self, endpoint, payload)
    }

    fn send_reliable(
        &self,
        endpoint: &crate::Endpoint,
        payload: &[u8],
    ) -> Result<(), crate::error::MessagingError> {
        NetTransport::send_reliable(self, endpoint, payload)
    }

    fn register(
        &self,
        token: crate::UID,
        receiver: Arc<dyn super::endpoint_map::MessageReceiver>,
    ) -> crate::Endpoint {
        NetTransport::register(self, token, receiver)
    }

    fn unregister(
        &self,
        token: &crate::UID,
    ) -> Option<Arc<dyn super::endpoint_map::MessageReceiver>> {
        NetTransport::unregister(self, token)
    }

    fn register_pending_reply(
        &self,
        addr: &str,
        token: crate::UID,
        closer: std::sync::Weak<dyn super::net_notified_queue::ReplyQueueCloser>,
    ) {
        NetTransport::register_pending_reply(self, addr, token, closer);
    }

    fn failure_monitor(&self) -> Arc<super::failure_monitor::FailureMonitor> {
        NetTransport::failure_monitor(self)
    }

    fn local_address(&self) -> &crate::NetworkAddress {
        NetTransport::local_address(self)
    }

    fn allocate_interface_token(&self) -> crate::UID {
        NetTransport::allocate_interface_token(self)
    }

    fn random_uid(&self) -> crate::UID {
        use crate::RandomProvider as _;
        crate::UID::new(
            self.providers.random().random::<u64>(),
            self.providers.random().random::<u64>(),
        )
    }

    fn is_local_address(&self, address: &crate::NetworkAddress) -> bool {
        NetTransport::is_local_address(self, address)
    }

    fn weak_for_cleanup(&self) -> std::sync::Weak<dyn super::transport_handle::TransportHandle> {
        self.weak_handle
            .read()
            .expect("RwLock poisoned: prior task panicked")
            .clone()
            .unwrap_or_else(|| std::sync::Weak::<Self>::new())
    }
}

impl<P: Providers + Send + Sync, C: MessageCodec> NetTransport<P, C> {
    /// Set the weak handle reference for `dyn TransportHandle` usage.
    ///
    /// Called by the builder after wrapping in `Arc`.
    pub(crate) fn set_weak_handle(
        &self,
        weak: std::sync::Weak<dyn super::transport_handle::TransportHandle>,
    ) {
        *self
            .weak_handle
            .write()
            .expect("RwLock poisoned: prior task panicked") = Some(weak);
    }
}

// =============================================================================
// NetTransportBuilder
// =============================================================================

/// Builder for `NetTransport` that eliminates common footguns.
///
/// The manual `Arc` wrapping and `set_weak_self()` pattern is error-prone:
/// forgetting `set_weak_self()` causes a runtime panic. This builder handles
/// both automatically.
///
/// # Examples
///
/// ```rust,ignore
/// // Standard usage - server or client that needs RPC responses:
/// let transport = NetTransportBuilder::new(providers)
///     .local_address(addr)
///     .build_listening()
///     .await?;
///
/// // Fire-and-forget sender (no listening needed):
/// let transport = NetTransportBuilder::new(providers)
///     .local_address(addr)
///     .build();
///
/// // With custom peer config:
/// let transport = NetTransportBuilder::new(providers)
///     .local_address(addr)
///     .peer_config(config)
///     .build_listening()
///     .await?;
/// ```
///
/// # Why Build vs Build Listening?
///
/// - **`build_listening()`**: For most RPC use cases. Both servers AND clients
///   need this because responses are sent to the client's listening address.
///
/// - **`build()`**: For fire-and-forget messaging where you don't expect responses.
///   Also useful for testing where you control message flow manually.
pub struct NetTransportBuilder<P: Providers, C: MessageCodec = crate::JsonCodec> {
    providers: P,
    codec: C,
    local_address: Option<NetworkAddress>,
    peer_config: Option<PeerConfig>,
}

impl<P: Providers> NetTransportBuilder<P, crate::JsonCodec> {
    /// Create a new builder with the providers bundle.
    ///
    /// Uses [`JsonCodec`](crate::JsonCodec) by default. Call [`.codec()`](Self::codec)
    /// to use a different codec.
    ///
    /// # Arguments
    ///
    /// * `providers` - Providers bundle for network, time, task, and random
    pub fn new(providers: P) -> Self {
        Self {
            providers,
            codec: crate::JsonCodec,
            local_address: None,
            peer_config: None,
        }
    }
}

/// A [`NetTransport`] backed by the production [`TokioProviders`](moonpool_core::TokioProviders)
/// bundle (and the default [`JsonCodec`](crate::JsonCodec)) — the typical
/// production transport type.
#[cfg(feature = "tokio")]
pub type TokioTransport = NetTransport<moonpool_core::TokioProviders>;

#[cfg(feature = "tokio")]
impl NetTransportBuilder<moonpool_core::TokioProviders, crate::JsonCodec> {
    /// Create a builder backed by the production
    /// [`TokioProviders`](moonpool_core::TokioProviders) bundle.
    ///
    /// Shorthand for `NetTransportBuilder::new(TokioProviders::new())` — the
    /// usual production entry point.
    #[must_use]
    pub fn tokio() -> Self {
        Self::new(moonpool_core::TokioProviders::new())
    }
}

impl<P: Providers + Send + Sync, C: MessageCodec> NetTransportBuilder<P, C> {
    /// Set the message codec for this transport.
    ///
    /// The codec is used for all serialization/deserialization in RPC handlers
    /// and client endpoints created from this transport.
    pub fn codec<NewC: MessageCodec>(self, codec: NewC) -> NetTransportBuilder<P, NewC> {
        NetTransportBuilder {
            providers: self.providers,
            codec,
            local_address: self.local_address,
            peer_config: self.peer_config,
        }
    }

    /// Set the local address for this transport.
    ///
    /// This is required before calling `build()` or `build_listening()`.
    ///
    /// # Arguments
    ///
    /// * `address` - The network address to bind to
    #[must_use]
    pub fn local_address(mut self, address: NetworkAddress) -> Self {
        self.local_address = Some(address);
        self
    }

    /// Set custom peer configuration.
    ///
    /// If not set, uses `PeerConfig::default()`.
    #[must_use]
    pub fn peer_config(mut self, config: PeerConfig) -> Self {
        self.peer_config = Some(config);
        self
    }

    /// Build the transport without starting the listener.
    ///
    /// Returns `Arc<NetTransport>` with `set_weak_self()` already called.
    /// Use this for fire-and-forget messaging or testing.
    ///
    /// For RPC (request/response), use `build_listening()` instead.
    ///
    /// # Errors
    ///
    /// Returns `MessagingError::MissingLocalAddress` if `local_address()` was not called.
    pub fn build(self) -> Result<Arc<NetTransport<P, C>>, MessagingError> {
        let address = self
            .local_address
            .ok_or(MessagingError::MissingLocalAddress)?;

        let mut transport = NetTransport::new(address, self.providers, self.codec);

        if let Some(config) = self.peer_config {
            transport = transport.with_peer_config(config);
        }

        let transport = Arc::new(transport);
        transport.set_weak_self(Arc::downgrade(&transport));
        let handle: Arc<dyn super::transport_handle::TransportHandle> =
            transport.clone() as Arc<dyn super::transport_handle::TransportHandle>;
        transport.set_weak_handle(Arc::downgrade(&handle));
        Ok(transport)
    }

    /// Build the transport and start listening for incoming connections.
    ///
    /// Returns `Arc<NetTransport>` with `set_weak_self()` already called
    /// and the listener started.
    ///
    /// Use this for typical RPC usage where you need to receive responses.
    /// Both servers AND clients need this in a request/response pattern.
    ///
    /// # Errors
    ///
    /// Returns `MessagingError::MissingLocalAddress` if `local_address()` was not called,
    /// or a network error if binding to the local address fails.
    pub async fn build_listening(self) -> Result<Arc<NetTransport<P, C>>, MessagingError> {
        let transport = self.build()?;
        transport.listen().await?;
        Ok(transport)
    }
}

/// FDB: `connectionReader()` - reads from connection and dispatches to endpoints.
///
/// This is a background task that:
/// 1. Takes ownership of the peer's receiver channel at startup
/// 2. Reads incoming packets from the channel (no peer lock held during await)
/// 3. Dispatches them to the appropriate endpoint via `transport.dispatch()`
///
/// # FDB Reference
/// From NetTransport.actor.cpp:1401-1602 connectionReader
///
/// The FDB version is more complex (handles `ConnectPacket`, protocol negotiation),
/// but the core loop is: read packets → scanPackets → deliver.
async fn connection_reader<P: Providers + Send + Sync, C: MessageCodec>(
    transport: Weak<NetTransport<P, C>>,
    peer: SharedPeer<P>,
    peer_addr: String,
) {
    tracing::debug!("connection_reader: started for peer {}", peer_addr);

    // Take ownership of the receiver at startup.
    // This avoids holding the peer lock across await points (critical safety fix).
    let mut receiver = {
        if let Some(rx) = peer
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .take_receiver()
        {
            rx
        } else {
            tracing::error!(
                "connection_reader: receiver already taken for peer {}",
                peer_addr
            );
            return;
        }
    }; // peer lock released here

    loop {
        // Await directly on the owned receiver - safe, no peer lock involved!
        if let Some((token, payload)) = receiver.recv().await {
            // Try to get transport reference
            let Some(transport) = transport.upgrade() else {
                tracing::debug!(
                    "connection_reader: transport dropped, exiting for peer {}",
                    peer_addr
                );
                break;
            };

            // FDB: deliver() - looks up endpoint and dispatches
            if let Err(e) = transport.dispatch(&token, &payload) {
                tracing::debug!(
                    "connection_reader: dispatch failed for token {}: {:?}",
                    token,
                    e
                );

                // FDB: send WLTOKEN_ENDPOINT_NOT_FOUND back to sender
                // (FlowTransport.cpp:1244-1225)
                if !token.is_well_known() {
                    let mut notification = [0u8; 16];
                    notification[0..8].copy_from_slice(&token.first.to_le_bytes());
                    notification[8..16].copy_from_slice(&token.second.to_le_bytes());
                    let _ = peer
                        .write()
                        .expect("RwLock poisoned: prior task panicked")
                        .send_unreliable(
                            crate::peer::core::ENDPOINT_NOT_FOUND_TOKEN,
                            &notification,
                        );
                }
            }
        } else {
            // Channel closed - peer disconnected or shutdown
            tracing::debug!("connection_reader: peer {} receiver closed", peer_addr);
            // Close all pending reply queues for this peer with MaybeDelivered
            if let Some(transport) = transport.upgrade() {
                transport.close_pending_replies(&peer_addr, &ReplyError::MaybeDelivered);
            }
            break;
        }
    }

    tracing::debug!("connection_reader: exiting for peer {}", peer_addr);
}

/// FDB: `listen()` - accept loop spawning connectionIncoming per connection.
///
/// This is a background task that:
/// 1. Accepts incoming connections
/// 2. Spawns `connection_incoming` for each accepted connection
/// 3. Exits gracefully when shutdown signal is received
///
/// # FDB Reference
/// From NetTransport.actor.cpp:1646-1676 listen
async fn listen_task<P: Providers + Send + Sync, C: MessageCodec>(
    transport: Weak<NetTransport<P, C>>,
    listener: <P::Network as NetworkProvider>::TcpListener,
    listen_addr: String,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tracing::debug!("listen_task: started on {}", listen_addr);

    loop {
        // Use select! to race between accept and shutdown signal
        tokio::select! {
            // Bias toward shutdown to ensure timely exit
            biased;

            // Check shutdown signal first
            result = shutdown_rx.changed() => {
                match result {
                    Ok(()) if *shutdown_rx.borrow() => {
                        tracing::debug!("listen_task: shutdown signal received, exiting for {}", listen_addr);
                        break;
                    }
                    Err(_) => {
                        // Channel closed means transport was dropped
                        tracing::debug!("listen_task: shutdown channel closed, exiting for {}", listen_addr);
                        break;
                    }
                    _ => {
                        // Value changed but not to true - continue
                    }
                }
            }

            // Accept next connection
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, peer_addr)) => {
                        tracing::debug!(
                            "listen_task: accepted connection from {} on {}",
                            peer_addr,
                            listen_addr
                        );

                        // Get transport reference
                        let Some(transport_rc) = transport.upgrade() else {
                            tracing::debug!("listen_task: transport dropped, exiting");
                            break;
                        };

                        // Handle the incoming connection (FDB: connectionIncoming)
                        connection_incoming(
                            Arc::downgrade(&transport_rc),
                            stream,
                            peer_addr,
                            &transport_rc,
                        );

                    }
                    Err(e) => {
                        tracing::warn!("listen_task: accept error on {}: {:?}", listen_addr, e);
                        // Continue accepting - transient errors are expected
                    }
                }
            }
        }
    }

    tracing::debug!("listen_task: exiting for {}", listen_addr);
}

/// FDB: `connectionIncoming()` - handles accepted connection.
///
/// This function:
/// 1. Creates a peer for the incoming connection
/// 2. Spawns a `connection_reader` for the peer
///
/// # FDB Reference
/// From NetTransport.actor.cpp:1604-1644 connectionIncoming
///
/// Note: FDB's connectionIncoming waits for `ConnectPacket` to identify the peer.
/// We simplify by using the peer address directly since `SimNetworkProvider`
/// already provides the peer address.
///
/// FDB Pattern: Use `Peer::new_incoming()` with the accepted stream, not `Peer::new()`.
/// This uses the already-established connection rather than trying to connect back.
fn connection_incoming<P: Providers + Send + Sync, C: MessageCodec>(
    transport_weak: Weak<NetTransport<P, C>>,
    stream: <P::Network as NetworkProvider>::TcpStream,
    peer_addr: String,
    transport: &NetTransport<P, C>,
) {
    tracing::debug!(
        "connection_incoming: handling connection from {}",
        peer_addr
    );

    // Check if we already have a peer for this address (FDB: getOrOpenPeer in connectionReader:1555)
    // For incoming connections, we store in incoming_peers to avoid conflicts with outgoing peers
    //
    // Note: Unlike outgoing peers which can be reused, incoming peers with new streams
    // should replace the old one since the old connection is stale.
    let peer = {
        let data = transport
            .data
            .read()
            .expect("RwLock poisoned: prior task panicked");
        if data.incoming_peers.contains_key(&peer_addr) {
            tracing::debug!(
                "connection_incoming: replacing stale incoming peer for {}",
                peer_addr
            );
        }
        drop(data); // Release borrow

        // FDB Pattern: Use Peer::new_incoming() with the accepted stream
        // (NetTransport.actor.cpp:1123 Peer::onIncomingConnection)
        // This uses the already-established connection rather than trying to connect back.
        let fm = Some(Arc::clone(
            &transport
                .data
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .failure_monitor,
        ));
        let peer = Peer::new_incoming(
            &transport.providers,
            peer_addr.clone(),
            stream,
            transport.peer_config.clone(),
            fm,
        );
        let peer = Arc::new(RwLock::new(peer));

        // Store in incoming_peers (replaces any existing stale peer)
        transport
            .data
            .write()
            .expect("RwLock poisoned: prior task panicked")
            .incoming_peers
            .insert(peer_addr.clone(), Arc::clone(&peer));

        tracing::debug!(
            "connection_incoming: created new incoming peer for {}",
            peer_addr
        );
        peer
    };

    // Spawn connection_reader to handle incoming packets
    drop(transport.providers.task().spawn_task(
        "connection_reader",
        connection_reader(transport_weak, peer, peer_addr),
    ));
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::{
        JsonCodec, NetNotifiedQueue, TokioRandomProvider, TokioStorageProvider, TokioTaskProvider,
        TokioTimeProvider,
    };

    // Simple mock network provider that fails all connections
    // (we only test local delivery, so connections are never actually made)
    #[derive(Clone)]
    struct MockNetworkProvider;

    // Dummy stream type for the mock
    struct DummyStream;

    impl futures::io::AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }
    }

    impl futures::io::AsyncWrite for DummyStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }
    }

    impl std::marker::Unpin for DummyStream {}

    // Dummy listener type for the mock
    struct DummyListener;

    impl crate::TcpListenerTrait for DummyListener {
        type TcpStream = DummyStream;

        async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
            Err(std::io::Error::other("dummy listener"))
        }

        fn local_addr(&self) -> std::io::Result<String> {
            Err(std::io::Error::other("dummy listener"))
        }
    }

    impl NetworkProvider for MockNetworkProvider {
        type TcpStream = DummyStream;
        type TcpListener = DummyListener;

        async fn bind(&self, _addr: &str) -> std::io::Result<Self::TcpListener> {
            Err(std::io::Error::other("mock bind"))
        }

        async fn connect(&self, _addr: &str) -> std::io::Result<Self::TcpStream> {
            Err(std::io::Error::other("mock connection"))
        }
    }

    /// Mock providers bundle for testing
    #[derive(Clone)]
    struct MockProviders {
        network: MockNetworkProvider,
        time: TokioTimeProvider,
        task: TokioTaskProvider,
        random: TokioRandomProvider,
        storage: TokioStorageProvider,
    }

    impl MockProviders {
        fn new() -> Self {
            Self {
                network: MockNetworkProvider,
                time: TokioTimeProvider::new(),
                task: TokioTaskProvider,
                random: TokioRandomProvider::new(),
                storage: TokioStorageProvider::new(),
            }
        }
    }

    impl Providers for MockProviders {
        type Network = MockNetworkProvider;
        type Time = TokioTimeProvider;
        type Task = TokioTaskProvider;
        type Random = TokioRandomProvider;
        type Storage = TokioStorageProvider;

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

    fn create_test_transport() -> NetTransport<MockProviders> {
        NetTransport::new(test_address(), MockProviders::new(), JsonCodec)
    }

    #[test]
    fn test_new_transport() {
        let transport = create_test_transport();

        assert_eq!(transport.peer_count(), 0);
        assert_eq!(transport.endpoint_count(), 0);
        assert_eq!(transport.packets_sent(), 0);
    }

    #[test]
    fn test_register_well_known() {
        let transport = create_test_transport();

        let queue: Arc<NetNotifiedQueue<String>> = Arc::new(NetNotifiedQueue::with_codec(
            Endpoint::new(test_address(), UID::new(1, 1)),
            JsonCodec,
        ));

        transport
            .register_well_known(WellKnownToken::Ping, queue)
            .expect("register should succeed");

        assert_eq!(transport.endpoint_count(), 1);
    }

    #[test]
    fn test_register_dynamic() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Arc<NetNotifiedQueue<String>> = Arc::new(NetNotifiedQueue::with_codec(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        let endpoint = transport.register(token, queue);

        assert_eq!(endpoint.token, token);
        assert_eq!(transport.endpoint_count(), 1);
    }

    #[test]
    fn test_local_delivery() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Arc<NetNotifiedQueue<String>> = Arc::new(NetNotifiedQueue::with_codec(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        let endpoint = transport.register(token, Arc::clone(&queue) as Arc<dyn MessageReceiver>);

        // Send to local endpoint
        let payload = br#""hello local""#;
        transport
            .send_unreliable(&endpoint, payload)
            .expect("send should succeed");

        assert_eq!(transport.packets_dispatched(), 1);
        assert_eq!(queue.try_recv(), Some("hello local".to_string()));
    }

    #[test]
    fn test_endpoint_not_found() {
        let transport = create_test_transport();

        let endpoint = Endpoint::new(test_address(), UID::new(999, 999));
        let payload = br#""test""#;

        let result = transport.send_unreliable(&endpoint, payload);
        assert!(matches!(
            result,
            Err(MessagingError::EndpointNotFound { .. })
        ));
        assert_eq!(transport.packets_undelivered(), 1);
    }

    #[test]
    fn test_unregister() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Arc<NetNotifiedQueue<String>> = Arc::new(NetNotifiedQueue::with_codec(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        transport.register(token, queue as Arc<dyn MessageReceiver>);
        assert_eq!(transport.endpoint_count(), 1);

        let removed = transport.unregister(&token);
        assert!(removed.is_some());
        assert_eq!(transport.endpoint_count(), 0);
    }

    #[test]
    fn test_dispatch() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Arc<NetNotifiedQueue<String>> = Arc::new(NetNotifiedQueue::with_codec(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        transport.register(token, Arc::clone(&queue) as Arc<dyn MessageReceiver>);

        // Dispatch directly
        let payload = br#""dispatched""#;
        transport
            .dispatch(&token, payload)
            .expect("dispatch should succeed");

        assert_eq!(queue.try_recv(), Some("dispatched".to_string()));
    }

    // =========================================================================
    // NetTransportBuilder tests
    // =========================================================================

    #[test]
    fn test_builder_build() {
        let transport = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        // Should be properly initialized
        assert_eq!(transport.peer_count(), 0);
        assert_eq!(transport.endpoint_count(), 0);
        assert_eq!(transport.packets_sent(), 0);
        assert_eq!(*transport.local_address(), test_address());
    }

    #[test]
    fn test_builder_weak_self_set() {
        let transport = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        // weak_self should be set - verify by checking it doesn't panic
        // This verifies the builder correctly calls set_weak_self()
        assert!(
            transport
                .weak_self
                .read()
                .expect("RwLock poisoned: prior task panicked")
                .is_some()
        );
    }

    #[test]
    fn test_builder_with_peer_config() {
        let config = PeerConfig::default();
        let transport = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .peer_config(config)
            .build()
            .expect("build should succeed");

        // Transport should be created (peer_config is internal, but creation succeeds)
        assert_eq!(transport.peer_count(), 0);
    }

    #[test]
    fn test_builder_missing_address_returns_error() {
        let result = NetTransportBuilder::new(MockProviders::new()).build();

        assert!(matches!(result, Err(MessagingError::MissingLocalAddress)));
    }

    #[test]
    fn test_builder_local_delivery_works() {
        // Verify the built transport functions correctly
        let transport = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        let token = UID::new(0x1234, 0x5678);
        let queue: Arc<NetNotifiedQueue<String>> = Arc::new(NetNotifiedQueue::with_codec(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        let endpoint = transport.register(token, Arc::clone(&queue) as Arc<dyn MessageReceiver>);

        // Send to local endpoint
        let payload = br#""builder test""#;
        transport
            .send_unreliable(&endpoint, payload)
            .expect("send should succeed");

        assert_eq!(transport.packets_dispatched(), 1);
        assert_eq!(queue.try_recv(), Some("builder test".to_string()));
    }

    #[test]
    fn test_register_handler() {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct TestRequest {
            value: i32,
        }

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct TestResponse {
            value: i32,
        }

        let transport = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        let token = UID::new(0xDEAD, 0xBEEF);
        let stream = NetTransport::register_handler::<TestRequest, TestResponse>(&transport, token);

        // Handler should be registered
        assert_eq!(transport.endpoint_count(), 1);

        // Endpoint should match
        assert_eq!(stream.endpoint().token, token);
        assert_eq!(stream.endpoint().address, test_address());
    }

    #[test]
    fn test_register_handler_at_multi_method() {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct AddRequest {
            a: i32,
            b: i32,
        }

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct AddResponse {
            result: i32,
        }

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct SubRequest {
            a: i32,
            b: i32,
        }

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        struct SubResponse {
            result: i32,
        }

        const CALC_INTERFACE: u64 = 0xCA1C;
        const METHOD_ADD: u64 = 0;
        const METHOD_SUB: u64 = 1;

        let transport = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build()
            .expect("build should succeed");

        // Register multiple handlers for the same interface
        let (add_stream, add_token) = NetTransport::register_handler_at::<AddRequest, AddResponse>(
            &transport,
            CALC_INTERFACE,
            METHOD_ADD,
        );
        let (sub_stream, sub_token) = NetTransport::register_handler_at::<SubRequest, SubResponse>(
            &transport,
            CALC_INTERFACE,
            METHOD_SUB,
        );

        // Both handlers should be registered
        assert_eq!(transport.endpoint_count(), 2);

        // Tokens should be deterministic
        assert_eq!(add_token, UID::new(CALC_INTERFACE, METHOD_ADD));
        assert_eq!(sub_token, UID::new(CALC_INTERFACE, METHOD_SUB));

        // Streams should have correct endpoints
        assert_eq!(add_stream.endpoint().token, add_token);
        assert_eq!(sub_stream.endpoint().token, sub_token);
    }

    // =========================================================================
    // Builder error path tests (Phase 12D Step 18)
    // =========================================================================

    #[tokio::test]
    async fn test_build_listening_bind_error() {
        // MockNetworkProvider returns error on bind(), simulating port already in use
        let result = NetTransportBuilder::new(MockProviders::new())
            .local_address(test_address())
            .build_listening()
            .await;

        // Should return NetworkError when bind fails
        assert!(matches!(
            result,
            Err(crate::error::MessagingError::NetworkError { .. })
        ));
    }

    // =========================================================================
    // EndpointNotFound notification tests
    // =========================================================================

    #[test]
    fn test_dispatch_not_found_for_well_known_token() {
        let transport = create_test_transport();

        // Dispatch to a well-known token that's not registered
        let well_known = UID::well_known(42);
        let payload = b"test";

        let result = transport.dispatch(&well_known, payload);
        assert!(matches!(
            result,
            Err(MessagingError::EndpointNotFound { .. })
        ));
        // The guard in connection_reader checks is_well_known() before
        // sending notification — well-known tokens should NOT trigger notifications.
        assert!(well_known.is_well_known());
    }

    #[test]
    fn test_dispatch_not_found_for_dynamic_token() {
        let transport = create_test_transport();

        // Dispatch to a dynamic (non-well-known) token that's not registered
        let dynamic_token = UID::new(0xCAFE, 0xBABE);
        let payload = b"test";

        let result = transport.dispatch(&dynamic_token, payload);
        assert!(matches!(
            result,
            Err(MessagingError::EndpointNotFound { .. })
        ));
        // Dynamic tokens SHOULD trigger the notification in connection_reader.
        assert!(!dynamic_token.is_well_known());
    }
}
