//! FlowTransport: Central transport coordinator (FDB pattern).
//!
//! Manages peer connections and dispatches incoming packets to endpoints.
//! Provides synchronous send API (FDB pattern: never await on send).
//!
//! # FDB Reference
//! From FlowTransport.actor.cpp:300-600, FlowTransport.h:195-314

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use moonpool_foundation::{
    Endpoint, NetworkAddress, NetworkProvider, Peer, PeerConfig, TaskProvider, TimeProvider, UID,
    WellKnownToken, sometimes_assert,
};

use super::endpoint_map::{EndpointMap, MessageReceiver};
use crate::error::MessagingError;

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

    /// Peer connections keyed by destination address.
    /// FDB: std::unordered_map<NetworkAddress, Reference<struct Peer>> peers;
    peers: HashMap<String, SharedPeer<N, T, TP>>,

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
    pub fn new(local_address: NetworkAddress, network: N, time: T, task_provider: TP) -> Self {
        Self {
            data: RefCell::new(TransportData::default()),
            local_address,
            network,
            time,
            task_provider,
            peer_config: PeerConfig::default(),
        }
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
    /// FDB: Reference<struct Peer> getOrOpenPeer(NetworkAddress const& address);
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

        let mut data = self.data.borrow_mut();
        data.peers.insert(addr_str, Rc::clone(&peer));
        data.stats.peers_created += 1;
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
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::NetNotifiedQueue;

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

        let queue: Rc<NetNotifiedQueue<String>> = Rc::new(NetNotifiedQueue::new(Endpoint::new(
            test_address(),
            UID::new(1, 1),
        )));

        transport
            .register_well_known(WellKnownToken::Ping, queue)
            .expect("register should succeed");

        assert_eq!(transport.endpoint_count(), 1);
    }

    #[test]
    fn test_register_dynamic() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Rc<NetNotifiedQueue<String>> =
            Rc::new(NetNotifiedQueue::new(Endpoint::new(test_address(), token)));

        let endpoint = transport.register(token, queue);

        assert_eq!(endpoint.token, token);
        assert_eq!(transport.endpoint_count(), 1);
    }

    #[test]
    fn test_local_delivery() {
        let transport = create_test_transport();

        let token = UID::new(0x1234, 0x5678);
        let queue: Rc<NetNotifiedQueue<String>> =
            Rc::new(NetNotifiedQueue::new(Endpoint::new(test_address(), token)));

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
        let queue: Rc<NetNotifiedQueue<String>> =
            Rc::new(NetNotifiedQueue::new(Endpoint::new(test_address(), token)));

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
        let queue: Rc<NetNotifiedQueue<String>> =
            Rc::new(NetNotifiedQueue::new(Endpoint::new(test_address(), token)));

        transport.register(token, Rc::clone(&queue) as Rc<dyn MessageReceiver>);

        // Dispatch directly
        let payload = br#""dispatched""#;
        transport
            .dispatch(&token, payload)
            .expect("dispatch should succeed");

        assert_eq!(queue.try_recv(), Some("dispatched".to_string()));
    }
}
