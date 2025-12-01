//! FlowTransport: Central transport coordinator (FDB pattern).
//!
//! Manages peer connections and dispatches incoming packets to endpoints.
//! Provides synchronous send API (FDB pattern: never await on send).
//!
//! # FDB Reference
//! From FlowTransport.actor.cpp:300-600, FlowTransport.h:195-314
//!
//! # Usage
//!
//! Use [`FlowTransportBuilder`] to create a properly configured transport:
//!
//! ```rust,ignore
//! // For servers and clients that need RPC responses:
//! let transport = FlowTransportBuilder::new(network, time, task)
//!     .local_address(addr)
//!     .build_listening()
//!     .await?;
//!
//! // For fire-and-forget senders only (no listening):
//! let transport = FlowTransportBuilder::new(network, time, task)
//!     .local_address(addr)
//!     .build();
//! ```
//!
//! The builder automatically handles `Rc` wrapping and `set_weak_self()`,
//! eliminating the most common footgun in FlowTransport usage.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use moonpool_foundation::{
    Endpoint, NetworkAddress, NetworkProvider, Peer, PeerConfig, TaskProvider, TcpListenerTrait,
    TimeProvider, UID, WellKnownToken, sometimes_assert,
};
use tokio::sync::watch;

use super::endpoint_map::{EndpointMap, MessageReceiver};
use super::request_stream::RequestStream;
use crate::error::MessagingError;
use moonpool_traits::MessageCodec;
use serde::de::DeserializeOwned;

/// Type alias for shared peer reference.
type SharedPeer<N, T, TP> = Rc<RefCell<Peer<N, T, TP>>>;

/// Internal transport data (FDB TransportData equivalent).
///
/// Separates internal mutable state from public API, matching FDB's pattern.
/// See FlowTransport.actor.cpp:300-350 for the original TransportData struct.
///
/// # FDB Reference
/// ```cpp
/// struct TransportData {
///     std::unordered_map<NetworkAddress, Reference<struct Peer>> peers;
///     std::unordered_map<NetworkAddress, std::pair<double, double>> closedPeers;
///     // ... endpoints, stats, etc.
/// };
/// ```
struct TransportData<N, T, TP>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
{
    /// Endpoint map for routing incoming packets.
    endpoints: EndpointMap,

    /// Peer connections keyed by destination address (outgoing).
    /// FDB: std::unordered_map<NetworkAddress, Reference<struct Peer>> peers;
    peers: HashMap<String, SharedPeer<N, T, TP>>,

    /// Incoming peer connections (from accepted connections).
    /// Separate from outgoing peers to avoid conflicts.
    incoming_peers: HashMap<String, SharedPeer<N, T, TP>>,

    /// Statistics.
    stats: TransportStats,
}

impl<N, T, TP> Default for TransportData<N, T, TP>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
{
    fn default() -> Self {
        Self {
            endpoints: EndpointMap::new(),
            peers: HashMap::new(),
            incoming_peers: HashMap::new(),
            stats: TransportStats::default(),
        }
    }
}

/// Central transport coordinator (FDB FlowTransport equivalent).
///
/// # Design
///
/// - Manages peer connections lazily (created on first send)
/// - Routes incoming packets to registered endpoints
/// - Synchronous send API - queues immediately, returns (FDB pattern)
/// - Explicit `Rc<FlowTransport>` passing (testable, simulation-friendly)
/// - Internal state in `TransportData` (matches FDB: `TransportData* self`)
///
/// # FDB Reference
/// From FlowTransport.h:195-314
///
/// # Multi-Node Support (Phase 12 Step 7d)
///
/// For multi-node operation, wrap in `Rc` and call `set_weak_self()`:
/// ```ignore
/// let transport = Rc::new(FlowTransport::new(...));
/// transport.set_weak_self(Rc::downgrade(&transport));
/// transport.listen().await?; // Start accepting connections
/// ```
pub struct FlowTransport<N, T, TP>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
{
    /// Internal transport data (FDB: TransportData* self).
    data: RefCell<TransportData<N, T, TP>>,

    /// Local address for this transport.
    local_address: NetworkAddress,

    /// Network provider for creating connections.
    network: N,

    /// Time provider for delays and timing.
    time: T,

    /// Task provider for spawning background tasks.
    task_provider: TP,

    /// Peer configuration.
    peer_config: PeerConfig,

    /// Weak self-reference for spawning background tasks.
    /// Required for `connection_reader` tasks to dispatch back to this transport.
    /// Set via `set_weak_self()` after wrapping in `Rc`.
    weak_self: RefCell<Option<Weak<Self>>>,

    /// Shutdown signal sender. When set to `true`, background tasks (listen_task) should exit.
    /// Uses watch channel so multiple receivers can observe the same signal.
    shutdown_tx: watch::Sender<bool>,
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

impl<N, T, TP> FlowTransport<N, T, TP>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    /// Create a new FlowTransport.
    ///
    /// # Arguments
    ///
    /// * `local_address` - Address of this transport (for local delivery checks)
    /// * `network` - Network provider for connections
    /// * `time` - Time provider for timing
    /// * `task_provider` - Task provider for background tasks
    ///
    /// # Multi-Node Usage
    ///
    /// For multi-node operation, wrap in `Rc` and call `set_weak_self()`:
    /// ```ignore
    /// let transport = Rc::new(FlowTransport::new(...));
    /// transport.set_weak_self(Rc::downgrade(&transport));
    /// ```
    pub fn new(local_address: NetworkAddress, network: N, time: T, task_provider: TP) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            data: RefCell::new(TransportData::default()),
            local_address,
            network,
            time,
            task_provider,
            peer_config: PeerConfig::default(),
            weak_self: RefCell::new(None),
            shutdown_tx,
        }
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
    /// let transport = Rc::new(FlowTransport::new(...));
    /// transport.set_weak_self(Rc::downgrade(&transport));
    /// ```
    pub fn set_weak_self(&self, weak: Weak<Self>) {
        *self.weak_self.borrow_mut() = Some(weak);
    }

    /// Get weak self-reference, panics if not set.
    fn weak_self(&self) -> Weak<Self> {
        self.weak_self
            .borrow()
            .clone()
            .expect("weak_self not set - call set_weak_self() after wrapping in Rc")
    }

    /// Create with custom peer configuration.
    pub fn with_peer_config(mut self, config: PeerConfig) -> Self {
        self.peer_config = config;
        self
    }

    /// Get the local address.
    pub fn local_address(&self) -> &NetworkAddress {
        &self.local_address
    }

    /// Register a well-known endpoint.
    ///
    /// Well-known endpoints have deterministic tokens for O(1) lookup.
    pub fn register_well_known(
        &self,
        token: WellKnownToken,
        receiver: Rc<dyn MessageReceiver>,
    ) -> Result<(), MessagingError> {
        self.data
            .borrow_mut()
            .endpoints
            .insert_well_known(token, receiver)
    }

    /// Register a dynamic endpoint with the given UID.
    ///
    /// Returns the endpoint that senders should use to address this receiver.
    pub fn register(&self, token: UID, receiver: Rc<dyn MessageReceiver>) -> Endpoint {
        self.data.borrow_mut().endpoints.insert(token, receiver);
        Endpoint::new(self.local_address.clone(), token)
    }

    /// Unregister a dynamic endpoint.
    pub fn unregister(&self, token: &UID) -> Option<Rc<dyn MessageReceiver>> {
        self.data.borrow_mut().endpoints.remove(token)
    }

    /// Register a typed request handler in a single step.
    ///
    /// This is the preferred method for registering RPC handlers. It combines:
    /// - Creating an endpoint from the local address and token
    /// - Creating a `RequestStream` for the request type
    /// - Registering the stream's queue with the transport
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Before (verbose):
    /// let endpoint = Endpoint::new(local_addr.clone(), ping_token());
    /// let stream: RequestStream<PingRequest, JsonCodec> =
    ///     RequestStream::new(endpoint, JsonCodec);
    /// transport.register(ping_token(), stream.queue() as Rc<dyn MessageReceiver>);
    ///
    /// // After (single call):
    /// let stream = transport.register_handler::<PingRequest>(ping_token(), JsonCodec);
    /// ```
    ///
    /// # Arguments
    ///
    /// * `token` - Unique identifier for this handler
    /// * `codec` - Codec for serializing/deserializing messages
    ///
    /// # Type Parameters
    ///
    /// * `Req` - The request type this handler will receive
    /// * `C` - The codec type (e.g., `JsonCodec`)
    pub fn register_handler<Req, C>(&self, token: UID, codec: C) -> RequestStream<Req, C>
    where
        Req: DeserializeOwned + 'static,
        C: MessageCodec,
    {
        let endpoint = Endpoint::new(self.local_address.clone(), token);
        let stream = RequestStream::new(endpoint, codec);
        self.data
            .borrow_mut()
            .endpoints
            .insert(token, stream.queue() as Rc<dyn MessageReceiver>);
        stream
    }

    /// Register a handler for a multi-method interface.
    ///
    /// Use this when an interface has multiple methods (like a Calculator with
    /// add, subtract, multiply, divide). Each method gets its own handler.
    ///
    /// The token is computed deterministically from `interface_id` and `method_index`,
    /// making it easy to create matching client endpoints.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Calculator interface with multiple methods
    /// const CALC_INTERFACE: u64 = 0xCA1C;
    /// const METHOD_ADD: u64 = 0;
    /// const METHOD_SUB: u64 = 1;
    /// const METHOD_MUL: u64 = 2;
    /// const METHOD_DIV: u64 = 3;
    ///
    /// let add_stream = transport.register_handler_at::<AddRequest>(
    ///     CALC_INTERFACE, METHOD_ADD, JsonCodec
    /// );
    /// let sub_stream = transport.register_handler_at::<SubRequest>(
    ///     CALC_INTERFACE, METHOD_SUB, JsonCodec
    /// );
    ///
    /// // Handle requests with tokio::select!
    /// loop {
    ///     tokio::select! {
    ///         Some((req, reply)) = add_stream.recv_with_transport(&transport) => {
    ///             reply.send(AddResponse { result: req.a + req.b });
    ///         }
    ///         Some((req, reply)) = sub_stream.recv_with_transport(&transport) => {
    ///             reply.send(SubResponse { result: req.a - req.b });
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// # Arguments
    ///
    /// * `interface_id` - Unique identifier for the interface
    /// * `method_index` - Index of the method within the interface (0, 1, 2, ...)
    /// * `codec` - Codec for serializing/deserializing messages
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - The `RequestStream` for receiving requests
    /// - The token (`UID`) that clients should use to send requests to this handler
    pub fn register_handler_at<Req, C>(
        &self,
        interface_id: u64,
        method_index: u64,
        codec: C,
    ) -> (RequestStream<Req, C>, UID)
    where
        Req: DeserializeOwned + 'static,
        C: MessageCodec,
    {
        let token = UID::new(interface_id, method_index);
        let stream = self.register_handler(token, codec);
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
    pub fn send_unreliable(
        &self,
        endpoint: &Endpoint,
        payload: &[u8],
    ) -> Result<(), MessagingError> {
        // Check for local delivery
        if self.is_local_address(&endpoint.address) {
            sometimes_assert!(
                local_delivery_path,
                true,
                "Unreliable send uses local delivery path"
            );
            return self.deliver_local(&endpoint.token, payload);
        }

        // Get or create peer for remote address
        sometimes_assert!(
            remote_peer_path,
            true,
            "Unreliable send uses remote peer path"
        );
        let peer = self.get_or_open_peer(&endpoint.address);
        peer.borrow_mut()
            .send_unreliable(endpoint.token, payload)
            .map_err(|e| MessagingError::PeerError {
                message: e.to_string(),
            })?;

        self.data.borrow_mut().stats.packets_sent += 1;
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
    pub fn send_reliable(&self, endpoint: &Endpoint, payload: &[u8]) -> Result<(), MessagingError> {
        // Check for local delivery
        if self.is_local_address(&endpoint.address) {
            return self.deliver_local(&endpoint.token, payload);
        }

        // Get or create peer for remote address
        let peer = self.get_or_open_peer(&endpoint.address);
        peer.borrow_mut()
            .send_reliable(endpoint.token, payload)
            .map_err(|e| MessagingError::PeerError {
                message: e.to_string(),
            })?;

        self.data.borrow_mut().stats.packets_sent += 1;
        Ok(())
    }

    /// Check if address is local (same as this transport).
    fn is_local_address(&self, address: &NetworkAddress) -> bool {
        self.local_address == *address
    }

    /// Deliver packet locally (same process, no network).
    fn deliver_local(&self, token: &UID, payload: &[u8]) -> Result<(), MessagingError> {
        let data = self.data.borrow();
        if let Some(receiver) = data.endpoints.get(token) {
            receiver.receive(payload);
            drop(data); // Release borrow before mutating stats
            self.data.borrow_mut().stats.packets_dispatched += 1;
            sometimes_assert!(
                endpoint_found_local,
                true,
                "Local delivery found registered endpoint"
            );
            Ok(())
        } else {
            drop(data); // Release borrow before mutating stats
            self.data.borrow_mut().stats.packets_undelivered += 1;
            sometimes_assert!(
                endpoint_not_found_local,
                true,
                "Local delivery to unregistered endpoint"
            );
            Err(MessagingError::EndpointNotFound { token: *token })
        }
    }

    /// Get or create a peer for the given address.
    ///
    /// Peers are created lazily on first send (FDB connectionKeeper pattern).
    /// When a new peer is created, a `connection_reader` task is spawned to
    /// handle incoming packets (FDB: connectionKeeper spawns connectionReader at line 843).
    ///
    /// # FDB Reference
    /// `Reference<struct Peer> getOrOpenPeer(NetworkAddress const& address);`
    fn get_or_open_peer(&self, address: &NetworkAddress) -> SharedPeer<N, T, TP> {
        let addr_str = address.to_string();

        // Check if peer already exists
        if let Some(peer) = self.data.borrow().peers.get(&addr_str) {
            sometimes_assert!(peer_reused, true, "Existing peer reused for address");
            return Rc::clone(peer);
        }

        // Create new peer
        let peer = Peer::new(
            self.network.clone(),
            self.time.clone(),
            self.task_provider.clone(),
            addr_str.clone(),
            self.peer_config.clone(),
        );
        let peer = Rc::new(RefCell::new(peer));

        // Store in peers map
        {
            let mut data = self.data.borrow_mut();
            data.peers.insert(addr_str.clone(), Rc::clone(&peer));
            data.stats.peers_created += 1;
        }

        // Spawn connection_reader for incoming packets (FDB pattern: connectionKeeper spawns connectionReader)
        // This handles responses for outgoing requests
        self.spawn_connection_reader(Rc::clone(&peer), addr_str);

        sometimes_assert!(peer_created, true, "New peer created for address");
        peer
    }

    /// Dispatch an incoming packet to the appropriate endpoint.
    ///
    /// Called by the transport loop when a packet is received.
    ///
    /// # Returns
    ///
    /// Ok(()) if delivered, Err if endpoint not found.
    pub fn dispatch(&self, token: &UID, payload: &[u8]) -> Result<(), MessagingError> {
        let data = self.data.borrow();
        if let Some(receiver) = data.endpoints.get(token) {
            receiver.receive(payload);
            drop(data); // Release borrow before mutating stats
            self.data.borrow_mut().stats.packets_dispatched += 1;
            sometimes_assert!(dispatch_success, true, "Message dispatched to endpoint");
            Ok(())
        } else {
            drop(data); // Release borrow before mutating stats
            self.data.borrow_mut().stats.packets_undelivered += 1;
            sometimes_assert!(
                dispatch_undelivered,
                true,
                "Dispatch to unregistered endpoint"
            );
            Err(MessagingError::EndpointNotFound { token: *token })
        }
    }

    /// Get statistics.
    pub fn packets_sent(&self) -> u64 {
        self.data.borrow().stats.packets_sent
    }

    pub fn packets_dispatched(&self) -> u64 {
        self.data.borrow().stats.packets_dispatched
    }

    pub fn packets_undelivered(&self) -> u64 {
        self.data.borrow().stats.packets_undelivered
    }

    pub fn peers_created(&self) -> u64 {
        self.data.borrow().stats.peers_created
    }

    /// Get number of active peers.
    pub fn peer_count(&self) -> usize {
        self.data.borrow().peers.len()
    }

    /// Get number of registered endpoints.
    pub fn endpoint_count(&self) -> usize {
        let data = self.data.borrow();
        data.endpoints.well_known_count() + data.endpoints.dynamic_count()
    }

    /// Get number of incoming peers (from accepted connections).
    pub fn incoming_peer_count(&self) -> usize {
        self.data.borrow().incoming_peers.len()
    }

    /// Spawn a connection_reader for a peer.
    ///
    /// FDB Pattern: connectionKeeper spawns connectionReader (line 843).
    /// The connection_reader reads from the peer and dispatches to endpoints.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer to read from
    /// * `peer_addr` - Address string for logging
    fn spawn_connection_reader(&self, peer: SharedPeer<N, T, TP>, peer_addr: String) {
        // Only spawn if weak_self is set (multi-node mode)
        if self.weak_self.borrow().is_none() {
            return;
        }

        let transport_weak = self.weak_self();
        self.task_provider.spawn_task(
            "connection_reader",
            connection_reader(transport_weak, peer, peer_addr),
        );
    }

    // =========================================================================
    // Server Listener Support (FDB: listen + connectionIncoming)
    // =========================================================================

    /// Start listening for incoming connections.
    ///
    /// FDB Pattern: `listen()` (FlowTransport.actor.cpp:1646-1676)
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
    /// let transport = Rc::new(FlowTransport::new(...));
    /// transport.set_weak_self(Rc::downgrade(&transport));
    /// transport.listen().await?;
    /// ```
    pub async fn listen(&self) -> Result<(), MessagingError> {
        // Verify weak_self is set
        if self.weak_self.borrow().is_none() {
            return Err(MessagingError::InvalidState {
                message: "weak_self not set - call set_weak_self() before listen()".to_string(),
            });
        }

        // Bind to local address
        let addr_str = self.local_address.to_string();
        let listener =
            self.network
                .bind(&addr_str)
                .await
                .map_err(|e| MessagingError::NetworkError {
                    message: format!("Failed to bind to {}: {}", addr_str, e),
                })?;

        tracing::info!("FlowTransport: listening on {}", addr_str);

        // Spawn listen task (FDB: listen() actor)
        // Pass shutdown receiver so the task can exit when transport is dropped
        let transport_weak = self.weak_self();
        let shutdown_rx = self.shutdown_tx.subscribe();
        self.task_provider.spawn_task(
            "listen",
            listen_task(transport_weak, listener, addr_str, shutdown_rx),
        );

        Ok(())
    }

    /// Get a shutdown receiver for background tasks.
    #[allow(dead_code)]
    fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }
}

/// Implement Drop to signal shutdown to background tasks.
///
/// When FlowTransport is dropped, we signal all background tasks (listen_task)
/// to exit gracefully. This prevents tasks from being stuck on accept() forever.
impl<N, T, TP> Drop for FlowTransport<N, T, TP>
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
{
    fn drop(&mut self) {
        tracing::debug!("FlowTransport: signaling shutdown to background tasks");
        // Signal shutdown - ignore error if no receivers
        let _ = self.shutdown_tx.send(true);
    }
}

// =============================================================================
// FlowTransportBuilder
// =============================================================================

/// Builder for FlowTransport that eliminates common footguns.
///
/// The manual `Rc` wrapping and `set_weak_self()` pattern is error-prone:
/// forgetting `set_weak_self()` causes a runtime panic. This builder handles
/// both automatically.
///
/// # Examples
///
/// ```rust,ignore
/// // Standard usage - server or client that needs RPC responses:
/// let transport = FlowTransportBuilder::new(network, time, task)
///     .local_address(addr)
///     .build_listening()
///     .await?;
///
/// // Fire-and-forget sender (no listening needed):
/// let transport = FlowTransportBuilder::new(network, time, task)
///     .local_address(addr)
///     .build();
///
/// // With custom peer config:
/// let transport = FlowTransportBuilder::new(network, time, task)
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
pub struct FlowTransportBuilder<N, T, TP>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    network: N,
    time: T,
    task_provider: TP,
    local_address: Option<NetworkAddress>,
    peer_config: Option<PeerConfig>,
}

impl<N, T, TP> FlowTransportBuilder<N, T, TP>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    /// Create a new builder with the required providers.
    ///
    /// # Arguments
    ///
    /// * `network` - Network provider for TCP connections
    /// * `time` - Time provider for timing operations
    /// * `task_provider` - Task provider for spawning background tasks
    pub fn new(network: N, time: T, task_provider: TP) -> Self {
        Self {
            network,
            time,
            task_provider,
            local_address: None,
            peer_config: None,
        }
    }

    /// Set the local address for this transport.
    ///
    /// This is required before calling `build()` or `build_listening()`.
    ///
    /// # Arguments
    ///
    /// * `address` - The network address to bind to
    pub fn local_address(mut self, address: NetworkAddress) -> Self {
        self.local_address = Some(address);
        self
    }

    /// Set custom peer configuration.
    ///
    /// If not set, uses `PeerConfig::default()`.
    pub fn peer_config(mut self, config: PeerConfig) -> Self {
        self.peer_config = Some(config);
        self
    }

    /// Build the transport without starting the listener.
    ///
    /// Returns `Rc<FlowTransport>` with `set_weak_self()` already called.
    /// Use this for fire-and-forget messaging or testing.
    ///
    /// For RPC (request/response), use `build_listening()` instead.
    ///
    /// # Panics
    ///
    /// Panics if `local_address()` was not called.
    pub fn build(self) -> Rc<FlowTransport<N, T, TP>> {
        let address = self
            .local_address
            .expect("local_address is required - call .local_address(addr) before .build()");

        let mut transport =
            FlowTransport::new(address, self.network, self.time, self.task_provider);

        if let Some(config) = self.peer_config {
            transport = transport.with_peer_config(config);
        }

        let transport = Rc::new(transport);
        transport.set_weak_self(Rc::downgrade(&transport));
        transport
    }

    /// Build the transport and start listening for incoming connections.
    ///
    /// Returns `Rc<FlowTransport>` with `set_weak_self()` already called
    /// and the listener started.
    ///
    /// Use this for typical RPC usage where you need to receive responses.
    /// Both servers AND clients need this in a request/response pattern.
    ///
    /// # Errors
    ///
    /// Returns an error if binding to the local address fails.
    ///
    /// # Panics
    ///
    /// Panics if `local_address()` was not called.
    pub async fn build_listening(self) -> Result<Rc<FlowTransport<N, T, TP>>, MessagingError> {
        let transport = self.build();
        transport.listen().await?;
        Ok(transport)
    }
}

/// FDB: connectionReader() - reads from connection and dispatches to endpoints.
///
/// This is a background task that:
/// 1. Takes ownership of the peer's receiver channel at startup
/// 2. Reads incoming packets from the channel (no RefCell borrow held during await)
/// 3. Dispatches them to the appropriate endpoint via `transport.dispatch()`
///
/// # FDB Reference
/// From FlowTransport.actor.cpp:1401-1602 connectionReader
///
/// The FDB version is more complex (handles ConnectPacket, protocol negotiation),
/// but the core loop is: read packets → scanPackets → deliver.
async fn connection_reader<N, T, TP>(
    transport: Weak<FlowTransport<N, T, TP>>,
    peer: SharedPeer<N, T, TP>,
    peer_addr: String,
) where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    tracing::debug!("connection_reader: started for peer {}", peer_addr);

    // Take ownership of the receiver at startup.
    // This avoids holding RefCell borrows across await points (critical safety fix).
    let mut receiver = {
        match peer.borrow_mut().take_receiver() {
            Some(rx) => rx,
            None => {
                tracing::error!(
                    "connection_reader: receiver already taken for peer {}",
                    peer_addr
                );
                return;
            }
        }
    }; // RefCell borrow released here

    loop {
        // Await directly on the owned receiver - safe, no RefCell involved!
        match receiver.recv().await {
            Some((token, payload)) => {
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
                }

                sometimes_assert!(
                    connection_reader_dispatch,
                    true,
                    "connectionReader dispatched incoming message"
                );
            }
            None => {
                // Channel closed - peer disconnected or shutdown
                tracing::debug!("connection_reader: peer {} receiver closed", peer_addr);
                break;
            }
        }
    }

    tracing::debug!("connection_reader: exiting for peer {}", peer_addr);
}

/// FDB: listen() - accept loop spawning connectionIncoming per connection.
///
/// This is a background task that:
/// 1. Accepts incoming connections
/// 2. Spawns `connection_incoming` for each accepted connection
/// 3. Exits gracefully when shutdown signal is received
///
/// # FDB Reference
/// From FlowTransport.actor.cpp:1646-1676 listen
async fn listen_task<N, T, TP>(
    transport: Weak<FlowTransport<N, T, TP>>,
    listener: N::TcpListener,
    listen_addr: String,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
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
                            Rc::downgrade(&transport_rc),
                            stream,
                            peer_addr,
                            &transport_rc,
                        );

                        sometimes_assert!(
                            connection_incoming_accepted,
                            true,
                            "listen() accepted incoming connection"
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

/// FDB: connectionIncoming() - handles accepted connection.
///
/// This function:
/// 1. Creates a peer for the incoming connection
/// 2. Spawns a connection_reader for the peer
///
/// # FDB Reference
/// From FlowTransport.actor.cpp:1604-1644 connectionIncoming
///
/// Note: FDB's connectionIncoming waits for ConnectPacket to identify the peer.
/// We simplify by using the peer address directly since SimNetworkProvider
/// already provides the peer address.
///
/// FDB Pattern: Use Peer::new_incoming() with the accepted stream, not Peer::new().
/// This uses the already-established connection rather than trying to connect back.
fn connection_incoming<N, T, TP>(
    transport_weak: Weak<FlowTransport<N, T, TP>>,
    stream: N::TcpStream,
    peer_addr: String,
    transport: &FlowTransport<N, T, TP>,
) where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
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
        let data = transport.data.borrow();
        if data.incoming_peers.contains_key(&peer_addr) {
            tracing::debug!(
                "connection_incoming: replacing stale incoming peer for {}",
                peer_addr
            );
        }
        drop(data); // Release borrow

        // FDB Pattern: Use Peer::new_incoming() with the accepted stream
        // (FlowTransport.actor.cpp:1123 Peer::onIncomingConnection)
        // This uses the already-established connection rather than trying to connect back.
        let peer = Peer::new_incoming(
            transport.network.clone(),
            transport.time.clone(),
            transport.task_provider.clone(),
            peer_addr.clone(),
            stream,
            transport.peer_config.clone(),
        );
        let peer = Rc::new(RefCell::new(peer));

        // Store in incoming_peers (replaces any existing stale peer)
        transport
            .data
            .borrow_mut()
            .incoming_peers
            .insert(peer_addr.clone(), Rc::clone(&peer));

        tracing::debug!(
            "connection_incoming: created new incoming peer for {}",
            peer_addr
        );
        peer
    };

    // Spawn connection_reader to handle incoming packets
    transport.task_provider.spawn_task(
        "connection_reader",
        connection_reader(transport_weak, peer, peer_addr),
    );
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::NetNotifiedQueue;
    use moonpool_traits::JsonCodec;

    // Use foundation's TokioNetworkProvider and TokioTimeProvider for testing
    use moonpool_foundation::{TokioTaskProvider, TokioTimeProvider};

    // Simple mock network provider that fails all connections
    // (we only test local delivery, so connections are never actually made)
    #[derive(Clone)]
    struct MockNetworkProvider;

    // Dummy stream type for the mock
    struct DummyStream;

    impl tokio::io::AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }
    }

    impl tokio::io::AsyncWrite for DummyStream {
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

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy stream")))
        }
    }

    impl std::marker::Unpin for DummyStream {}

    // Dummy listener type for the mock
    struct DummyListener;

    #[async_trait::async_trait(?Send)]
    impl moonpool_foundation::TcpListenerTrait for DummyListener {
        type TcpStream = DummyStream;

        async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
            Err(std::io::Error::other("dummy listener"))
        }

        fn local_addr(&self) -> std::io::Result<String> {
            Err(std::io::Error::other("dummy listener"))
        }
    }

    #[async_trait::async_trait(?Send)]
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

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500)
    }

    fn create_test_transport()
    -> FlowTransport<MockNetworkProvider, TokioTimeProvider, TokioTaskProvider> {
        FlowTransport::new(
            test_address(),
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
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

        let queue: Rc<NetNotifiedQueue<String, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
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
        let queue: Rc<NetNotifiedQueue<String, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
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
        let queue: Rc<NetNotifiedQueue<String, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        let endpoint = transport.register(token, Rc::clone(&queue) as Rc<dyn MessageReceiver>);

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
        let queue: Rc<NetNotifiedQueue<String, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        transport.register(token, queue as Rc<dyn MessageReceiver>);
        assert_eq!(transport.endpoint_count(), 1);

        let removed = transport.unregister(&token);
        assert!(removed.is_some());
        assert_eq!(transport.endpoint_count(), 0);
    }

    #[test]
    fn test_dispatch() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Rc<NetNotifiedQueue<String, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        transport.register(token, Rc::clone(&queue) as Rc<dyn MessageReceiver>);

        // Dispatch directly
        let payload = br#""dispatched""#;
        transport
            .dispatch(&token, payload)
            .expect("dispatch should succeed");

        assert_eq!(queue.try_recv(), Some("dispatched".to_string()));
    }

    // =========================================================================
    // FlowTransportBuilder tests
    // =========================================================================

    #[test]
    fn test_builder_build() {
        let transport = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .build();

        // Should be properly initialized
        assert_eq!(transport.peer_count(), 0);
        assert_eq!(transport.endpoint_count(), 0);
        assert_eq!(transport.packets_sent(), 0);
        assert_eq!(*transport.local_address(), test_address());
    }

    #[test]
    fn test_builder_weak_self_set() {
        let transport = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .build();

        // weak_self should be set - verify by checking it doesn't panic
        // This verifies the builder correctly calls set_weak_self()
        assert!(transport.weak_self.borrow().is_some());
    }

    #[test]
    fn test_builder_with_peer_config() {
        let config = PeerConfig::default();
        let transport = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .peer_config(config)
        .build();

        // Transport should be created (peer_config is internal, but creation succeeds)
        assert_eq!(transport.peer_count(), 0);
    }

    #[test]
    #[should_panic(expected = "local_address is required")]
    fn test_builder_missing_address_panics() {
        let _ = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .build();
    }

    #[test]
    fn test_builder_local_delivery_works() {
        // Verify the built transport functions correctly
        let transport = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .build();

        let token = UID::new(0x1234, 0x5678);
        let queue: Rc<NetNotifiedQueue<String, JsonCodec>> = Rc::new(NetNotifiedQueue::new(
            Endpoint::new(test_address(), token),
            JsonCodec,
        ));

        let endpoint = transport.register(token, Rc::clone(&queue) as Rc<dyn MessageReceiver>);

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

        let transport = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .build();

        let token = UID::new(0xDEAD, 0xBEEF);
        let stream = transport.register_handler::<TestRequest, _>(token, JsonCodec);

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
        struct SubRequest {
            a: i32,
            b: i32,
        }

        const CALC_INTERFACE: u64 = 0xCA1C;
        const METHOD_ADD: u64 = 0;
        const METHOD_SUB: u64 = 1;

        let transport = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .build();

        // Register multiple handlers for the same interface
        let (add_stream, add_token) =
            transport.register_handler_at::<AddRequest, _>(CALC_INTERFACE, METHOD_ADD, JsonCodec);
        let (sub_stream, sub_token) =
            transport.register_handler_at::<SubRequest, _>(CALC_INTERFACE, METHOD_SUB, JsonCodec);

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
        let result = FlowTransportBuilder::new(
            MockNetworkProvider,
            TokioTimeProvider::new(),
            TokioTaskProvider,
        )
        .local_address(test_address())
        .build_listening()
        .await;

        // Should return NetworkError when bind fails
        assert!(matches!(
            result,
            Err(crate::error::MessagingError::NetworkError { .. })
        ));
    }
}
