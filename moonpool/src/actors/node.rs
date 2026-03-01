//! MoonpoolNode: unified actor runtime for a single node.
//!
//! [`MoonpoolNode`] ties together transport, directory, membership, and actor
//! hosting into a single entry point. It creates the transport and router
//! internally and manages the full lifecycle.
//!
//! # Builder API
//!
//! ```rust,ignore
//! let node = MoonpoolNode::join(cluster)
//!     .with_providers(providers)
//!     .with_address(addr)
//!     .with_placement(placement)
//!     .register::<CounterActor>()
//!     .start()
//!     .await?;
//!
//! // Use actors via the node
//! let resp: ValueResponse = node.router().send_actor_request(
//!     &ActorId::new(COUNTER_TYPE, "my-counter"),
//!     1,
//!     &IncrementRequest { amount: 1 },
//! ).await?;
//!
//! node.shutdown().await?;
//! ```
//!
//! # Lifecycle
//!
//! 1. **Initializing**: Transport created, listener bound, router set up
//! 2. **Active**: Actor registrations executed, node serving requests
//! 3. **Stopping**: Request streams closed, pending tasks drained

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::{
    Endpoint, JsonCodec, MessageCodec, NetTransport, NetTransportBuilder, NetworkAddress,
    Providers, TaskProvider, UID,
};

use super::cluster::ClusterConfig;
use super::host::{ActorHandler, ActorTypeDispatcher, TypedDispatcher};
use super::router::{ActorError, ActorRouter};
use super::state::ActorStateStore;
use super::{ActorDirectory, PlacementStrategy};

/// Type alias for a collection of close handles (one per registered actor type).
type CloseHandles = Vec<Box<dyn Fn()>>;

/// Type alias for a registration closure that sets up an actor type.
type RegistrationFn<P, C> = Box<dyn FnOnce(&NodeParts<P, C>)>;

/// Lifecycle state of a [`MoonpoolNode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Transport created, registrations pending.
    Initializing,
    /// Node is serving requests and processing actor messages.
    Active,
    /// Shutting down: draining pending tasks.
    Stopping,
}

/// Unified actor runtime for a single node.
///
/// Owns the transport, creates the router internally, and manages actor
/// registration and lifecycle. Use [`MoonpoolNode::join`] to start building.
///
/// # Type Parameters
///
/// * `P` - The providers bundle (simulation or production)
/// * `C` - The message codec (defaults to [`JsonCodec`])
pub struct MoonpoolNode<P: Providers, C: MessageCodec = JsonCodec> {
    transport: Rc<NetTransport<P>>,
    router: Rc<ActorRouter<P, C>>,
    cluster: ClusterConfig,
    address: NetworkAddress,
    status: NodeStatus,
    pending_tasks: Rc<Cell<usize>>,
    close_handles: RefCell<CloseHandles>,
    providers: P,
}

impl<P: Providers, C: MessageCodec> MoonpoolNode<P, C> {
    /// Get a reference to the actor router.
    ///
    /// Use this to send requests to virtual actors via
    /// [`ActorRouter::send_actor_request`].
    pub fn router(&self) -> &Rc<ActorRouter<P, C>> {
        &self.router
    }

    /// Get a reference to the underlying transport.
    pub fn transport(&self) -> &Rc<NetTransport<P>> {
        &self.transport
    }

    /// This node's network address.
    pub fn address(&self) -> &NetworkAddress {
        &self.address
    }

    /// Current lifecycle status.
    pub fn status(&self) -> NodeStatus {
        self.status
    }

    /// The cluster configuration this node belongs to.
    pub fn cluster(&self) -> &ClusterConfig {
        &self.cluster
    }

    /// Gracefully shut down the node.
    ///
    /// Closes all actor request streams, waits for pending tasks to
    /// drain (actors run `on_deactivate`), then marks the node as stopped.
    ///
    /// After shutdown, the node can no longer process actor messages.
    pub async fn shutdown(&mut self) -> Result<(), ActorError> {
        self.status = NodeStatus::Stopping;

        // Close all request streams
        for close_fn in self.close_handles.borrow().iter() {
            close_fn();
        }

        // Yield until all processing loops finish
        while self.pending_tasks.get() > 0 {
            self.providers.task().yield_now().await;
        }

        Ok(())
    }
}

impl<P: Providers, C: MessageCodec> Drop for MoonpoolNode<P, C> {
    fn drop(&mut self) {
        for close_fn in self.close_handles.borrow().iter() {
            close_fn();
        }
    }
}

/// Builder for [`MoonpoolNode`].
///
/// Collects configuration and actor registrations, then creates the node
/// during [`start()`](Self::start).
///
/// # Example
///
/// ```rust,ignore
/// let node = MoonpoolNode::join(cluster)
///     .with_providers(providers)
///     .with_address(addr)
///     .with_placement(placement)
///     .register::<MyActor>()
///     .start()
///     .await?;
/// ```
pub struct MoonpoolNodeBuilder<P: Providers, C: MessageCodec = JsonCodec> {
    cluster: ClusterConfig,
    providers: Option<P>,
    address: Option<NetworkAddress>,
    placement: Option<Rc<dyn PlacementStrategy>>,
    codec: C,
    state_store: Option<Rc<dyn ActorStateStore>>,
    registrations: Vec<RegistrationFn<P, C>>,
}

/// Internal bag of references passed to registration closures during start().
struct NodeParts<P: Providers, C: MessageCodec> {
    transport: Rc<NetTransport<P>>,
    router: Rc<ActorRouter<P, C>>,
    directory: Rc<dyn ActorDirectory>,
    state_store: Option<Rc<dyn ActorStateStore>>,
    providers: P,
    pending_tasks: Rc<Cell<usize>>,
    close_handles: Rc<RefCell<CloseHandles>>,
}

impl<P: Providers> MoonpoolNode<P> {
    /// Start building a node that joins the given cluster.
    ///
    /// Uses [`JsonCodec`] by default. Call [`MoonpoolNodeBuilder::with_codec`]
    /// to override.
    pub fn join(cluster: ClusterConfig) -> MoonpoolNodeBuilder<P> {
        MoonpoolNodeBuilder {
            cluster,
            providers: None,
            address: None,
            placement: None,
            codec: JsonCodec,
            state_store: None,
            registrations: Vec::new(),
        }
    }
}

impl<P: Providers, C: MessageCodec> MoonpoolNodeBuilder<P, C> {
    /// Set the providers bundle (required).
    pub fn with_providers(mut self, providers: P) -> Self {
        self.providers = Some(providers);
        self
    }

    /// Set the node's network address (required).
    pub fn with_address(mut self, address: NetworkAddress) -> Self {
        self.address = Some(address);
        self
    }

    /// Set the placement strategy.
    ///
    /// If not set, a [`LocalPlacement`](super::LocalPlacement) is used.
    pub fn with_placement(mut self, placement: Rc<dyn PlacementStrategy>) -> Self {
        self.placement = Some(placement);
        self
    }

    /// Set a custom message codec.
    pub fn with_codec<C2: MessageCodec>(self, codec: C2) -> MoonpoolNodeBuilder<P, C2> {
        MoonpoolNodeBuilder {
            cluster: self.cluster,
            providers: self.providers,
            address: self.address,
            placement: self.placement,
            codec,
            state_store: self.state_store,
            registrations: Vec::new(), // type changed, must re-register
        }
    }

    /// Set an actor state store for persistent actor state.
    pub fn with_state_store(mut self, store: Rc<dyn ActorStateStore>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Register an actor handler type.
    ///
    /// The actual registration (spawning processing loops) happens during
    /// [`start()`](Self::start). Multiple actor types can be registered.
    pub fn register<H: ActorHandler>(mut self) -> Self {
        self.registrations.push(Box::new(|parts: &NodeParts<P, C>| {
            let dispatcher = TypedDispatcher::<H>::new();
            let close_handle = dispatcher.start(
                parts.transport.clone(),
                parts.router.clone(),
                parts.directory.clone(),
                parts.state_store.clone(),
                parts.providers.clone(),
                parts.pending_tasks.clone(),
            );
            parts.close_handles.borrow_mut().push(close_handle);
        }));
        self
    }

    /// Build and start the node.
    ///
    /// Creates the transport, router, and executes all actor registrations.
    /// The node is ready to process messages after this returns.
    ///
    /// # Errors
    ///
    /// Returns an error if required fields are missing or transport creation fails.
    pub async fn start(self) -> Result<MoonpoolNode<P, C>, NodeError> {
        let providers = self.providers.ok_or(NodeError::MissingProviders)?;
        let address = self.address.ok_or(NodeError::MissingAddress)?;

        // Create transport
        let transport = NetTransportBuilder::new(providers.clone())
            .local_address(address.clone())
            .build()
            .map_err(|e| NodeError::Transport(e.to_string()))?;

        // Determine placement strategy
        let placement = self.placement.unwrap_or_else(|| {
            let local_endpoint = Endpoint::new(address.clone(), UID::new(0, 0));
            Rc::new(super::LocalPlacement::new(local_endpoint))
        });

        // Create router
        let directory = self.cluster.directory().clone();
        let router = Rc::new(ActorRouter::new(
            transport.clone(),
            directory.clone(),
            placement,
            self.codec,
        ));

        let pending_tasks = Rc::new(Cell::new(0));
        let close_handles: Rc<RefCell<CloseHandles>> = Rc::new(RefCell::new(Vec::new()));

        // Execute registrations
        let parts = NodeParts {
            transport: transport.clone(),
            router: router.clone(),
            directory,
            state_store: self.state_store,
            providers: providers.clone(),
            pending_tasks: pending_tasks.clone(),
            close_handles: close_handles.clone(),
        };

        for registration in self.registrations {
            registration(&parts);
        }

        // Drain close handles from the shared RefCell into an owned collection.
        let close_handles_owned = RefCell::new(std::mem::take(&mut *close_handles.borrow_mut()));

        Ok(MoonpoolNode {
            transport,
            router,
            cluster: self.cluster,
            address,
            status: NodeStatus::Active,
            pending_tasks,
            close_handles: close_handles_owned,
            providers,
        })
    }
}

/// Errors from [`MoonpoolNode`] operations.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// Providers were not set on the builder.
    #[error("node requires providers (call with_providers())")]
    MissingProviders,

    /// Network address was not set on the builder.
    #[error("node requires an address (call with_address())")]
    MissingAddress,

    /// Transport creation failed.
    #[error("transport error: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use serde::{Deserialize, Serialize};

    use crate::actors::types::{ActorId, ActorType};
    use crate::actors::{InMemoryDirectory, LocalPlacement};
    use crate::{Endpoint, TokioProviders, UID};

    use super::super::host::{ActorContext, ActorHandler};
    use super::super::router::ActorError;
    use super::*;

    const TEST_ACTOR_TYPE: ActorType = ActorType(0x7E57_AC70);

    mod test_methods {
        pub const INCREMENT: u32 = 1;
        pub const GET_VALUE: u32 = 2;
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct IncrementRequest {
        amount: i64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct GetValueRequest {}

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct ValueResponse {
        value: i64,
    }

    #[derive(Default)]
    struct CounterActor {
        value: i64,
    }

    #[async_trait::async_trait(?Send)]
    impl ActorHandler for CounterActor {
        fn actor_type() -> ActorType {
            TEST_ACTOR_TYPE
        }

        async fn dispatch<P2: Providers, C2: MessageCodec>(
            &mut self,
            _ctx: &ActorContext<P2, C2>,
            method: u32,
            body: &[u8],
        ) -> Result<Vec<u8>, ActorError> {
            let codec = JsonCodec;
            match method {
                test_methods::INCREMENT => {
                    let req: IncrementRequest = codec.decode(body)?;
                    self.value += req.amount;
                    Ok(codec.encode(&ValueResponse { value: self.value })?)
                }
                test_methods::GET_VALUE => Ok(codec.encode(&ValueResponse { value: self.value })?),
                _ => Err(ActorError::UnknownMethod(method)),
            }
        }
    }

    fn addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    fn run_local_test<F: std::future::Future<Output = ()> + 'static>(f: F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .expect("build local runtime");
        rt.block_on(f);
    }

    #[test]
    fn test_node_builder_missing_providers() {
        run_local_test(async {
            let cluster = ClusterConfig::single_node(addr(4700));
            let result = MoonpoolNode::<TokioProviders>::join(cluster)
                .with_address(addr(4700))
                .start()
                .await;
            assert!(matches!(result, Err(NodeError::MissingProviders)));
        });
    }

    #[test]
    fn test_node_builder_missing_address() {
        run_local_test(async {
            let cluster = ClusterConfig::single_node(addr(4700));
            let result = MoonpoolNode::join(cluster)
                .with_providers(TokioProviders::new())
                .start()
                .await;
            assert!(matches!(result, Err(NodeError::MissingAddress)));
        });
    }

    #[test]
    fn test_node_start_and_send() {
        run_local_test(async {
            let local_addr = addr(4700);
            let directory: Rc<dyn ActorDirectory> = Rc::new(InMemoryDirectory::new());
            let local_endpoint = Endpoint::new(local_addr.clone(), UID::new(TEST_ACTOR_TYPE.0, 0));
            let placement: Rc<dyn PlacementStrategy> = Rc::new(LocalPlacement::new(local_endpoint));

            let cluster = ClusterConfig::builder()
                .directory(directory)
                .membership(Rc::new(super::super::membership::SharedMembership::new(
                    vec![local_addr.clone()],
                )))
                .build()
                .expect("build cluster");

            let mut node = MoonpoolNode::join(cluster)
                .with_providers(TokioProviders::new())
                .with_address(local_addr)
                .with_placement(placement)
                .register::<CounterActor>()
                .start()
                .await
                .expect("start node");

            assert_eq!(node.status(), NodeStatus::Active);

            tokio::task::yield_now().await;

            let actor_id = ActorId::new(TEST_ACTOR_TYPE, "test1");
            let resp: ValueResponse = node
                .router()
                .send_actor_request(
                    &actor_id,
                    test_methods::INCREMENT,
                    &IncrementRequest { amount: 42 },
                )
                .await
                .expect("send_actor_request should succeed");

            assert_eq!(resp, ValueResponse { value: 42 });

            node.shutdown().await.expect("shutdown");
            assert_eq!(node.status(), NodeStatus::Stopping);
        });
    }

    #[test]
    fn test_node_single_node_convenience() {
        run_local_test(async {
            let local_addr = addr(4701);
            let mut node = MoonpoolNode::join(ClusterConfig::single_node(local_addr.clone()))
                .with_providers(TokioProviders::new())
                .with_address(local_addr.clone())
                .register::<CounterActor>()
                .start()
                .await
                .expect("start node");

            tokio::task::yield_now().await;

            let actor_id = ActorId::new(TEST_ACTOR_TYPE, "counter1");
            let resp: ValueResponse = node
                .router()
                .send_actor_request(&actor_id, test_methods::GET_VALUE, &GetValueRequest {})
                .await
                .expect("get value");

            assert_eq!(resp, ValueResponse { value: 0 });

            node.shutdown().await.expect("shutdown");
        });
    }

    #[test]
    fn test_node_address_accessor() {
        run_local_test(async {
            let local_addr = addr(4702);
            let node = MoonpoolNode::join(ClusterConfig::single_node(local_addr.clone()))
                .with_providers(TokioProviders::new())
                .with_address(local_addr.clone())
                .start()
                .await
                .expect("start node");

            assert_eq!(node.address(), &local_addr);
        });
    }
}
