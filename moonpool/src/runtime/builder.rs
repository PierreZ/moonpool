//! Actor runtime builder for configuration.

use crate::actor::NodeId;
use crate::directory::SimpleDirectory;
use crate::error::ActorError;
use crate::messaging::{FoundationTransport, MessageBus};
use crate::runtime::ActorRuntime;
use moonpool_foundation::{
    NetworkProvider, PeerConfig, TaskProvider, TimeProvider,
    network::transport::{ClientTransport, Envelope, RequestResponseSerializer, ServerTransport},
};
use std::net::SocketAddr;
use std::rc::Rc;

/// Builder for ActorRuntime with fluent API.
///
/// Generic over provider types to enable simulation vs production configurations.
///
/// # Type Parameters
///
/// - `N`: NetworkProvider (e.g., TokioNetworkProvider, SimNetworkProvider)
/// - `T`: TimeProvider (e.g., TokioTimeProvider, SimTimeProvider)
/// - `TP`: TaskProvider (e.g., TokioTaskProvider)
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::ActorRuntime;
/// use moonpool::directory::SimpleDirectory;
/// use moonpool_foundation::{TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider};
///
/// // Create directory with cluster nodes
/// let nodes = vec![
///     NodeId::from("127.0.0.1:5000")?,
///     NodeId::from("127.0.0.1:5001")?,
/// ];
/// let directory = SimpleDirectory::new(nodes);
///
/// // Production configuration with Tokio providers
/// let runtime = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5000")
///     .directory(directory)
///     .with_providers(
///         TokioNetworkProvider,
///         TokioTimeProvider,
///         TokioTaskProvider,
///     )
///     .build()
///     .await?;
/// ```
pub struct ActorRuntimeBuilder<N, T, TP> {
    /// Cluster namespace (required).
    pub(crate) namespace: Option<String>,

    /// Listening address for incoming actor messages (required).
    pub(crate) listen_addr: Option<SocketAddr>,

    /// Directory for actor placement (required).
    pub(crate) directory: Option<SimpleDirectory>,

    /// Network provider (required).
    pub(crate) network_provider: N,

    /// Time provider (required).
    pub(crate) time_provider: T,

    /// Task provider (required).
    pub(crate) task_provider: TP,
}

impl ActorRuntimeBuilder<(), (), ()> {
    /// Create a new runtime builder.
    ///
    /// You must call `with_providers()` to set the network, time, and task providers
    /// before calling `build()`.
    pub fn new() -> Self {
        Self {
            namespace: None,
            listen_addr: None,
            directory: None,
            network_provider: (),
            time_provider: (),
            task_provider: (),
        }
    }
}

impl<N, T, TP> ActorRuntimeBuilder<N, T, TP> {
    /// Set the providers for network, time, and task execution.
    ///
    /// Required before calling `build()`. This method consumes the builder and returns
    /// a new builder with the providers set.
    ///
    /// # Type Parameters
    ///
    /// - `N`: NetworkProvider (e.g., TokioNetworkProvider, SimNetworkProvider)
    /// - `T`: TimeProvider (e.g., TokioTimeProvider, SimTimeProvider)
    /// - `TP`: TaskProvider (e.g., TokioTaskProvider)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool_foundation::{TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider};
    ///
    /// let runtime = ActorRuntime::builder()
    ///     .namespace("prod")
    ///     .listen_addr("127.0.0.1:5000")?
    ///     .directory(directory)
    ///     .with_providers(
    ///         TokioNetworkProvider,
    ///         TokioTimeProvider,
    ///         TokioTaskProvider,
    ///     )
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_providers<NewN, NewT, NewTP>(
        self,
        network_provider: NewN,
        time_provider: NewT,
        task_provider: NewTP,
    ) -> ActorRuntimeBuilder<NewN, NewT, NewTP> {
        ActorRuntimeBuilder {
            namespace: self.namespace,
            listen_addr: self.listen_addr,
            directory: self.directory,
            network_provider,
            time_provider,
            task_provider,
        }
    }

    /// Set the cluster namespace.
    ///
    /// All actors in this runtime will have this namespace prefix.
    /// Cannot communicate across namespace boundaries.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// builder.namespace("prod")
    /// builder.namespace("dev")
    /// builder.namespace("tenant-123")
    /// ```
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Set the listening address for this node.
    ///
    /// This node will bind to this address to receive actor messages.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// builder.listen_addr("127.0.0.1:5000")
    /// builder.listen_addr("0.0.0.0:8001")
    /// ```
    pub fn listen_addr(mut self, addr: impl AsRef<str>) -> std::result::Result<Self, ActorError> {
        let addr_str = addr.as_ref();
        let socket_addr: SocketAddr = addr_str.parse().map_err(|e| {
            ActorError::InvalidConfiguration(format!("Invalid listen_addr '{}': {}", addr_str, e))
        })?;
        self.listen_addr = Some(socket_addr);
        Ok(self)
    }

    /// Set the directory for actor placement.
    ///
    /// The directory tracks which nodes host which actors and provides
    /// placement decisions for new activations.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use moonpool::directory::SimpleDirectory;
    ///
    /// let nodes = vec![
    ///     NodeId::from("127.0.0.1:5000")?,
    ///     NodeId::from("127.0.0.1:5001")?,
    /// ];
    /// let directory = SimpleDirectory::new(nodes);
    /// builder.directory(directory)
    /// ```
    pub fn directory(mut self, directory: SimpleDirectory) -> Self {
        self.directory = Some(directory);
        self
    }
}

impl<N, T, TP> ActorRuntimeBuilder<N, T, TP>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    /// Build the ActorRuntime.
    ///
    /// Validates configuration and creates the runtime instance with network transport enabled.
    ///
    /// # Errors
    ///
    /// Returns error if required fields are missing or invalid.
    pub async fn build(self) -> std::result::Result<ActorRuntime<TP>, ActorError> {
        // Validate required fields
        let namespace = self
            .namespace
            .ok_or_else(|| ActorError::InvalidConfiguration("namespace is required".to_string()))?;

        let listen_addr = self.listen_addr.ok_or_else(|| {
            ActorError::InvalidConfiguration("listen_addr is required".to_string())
        })?;

        let directory = self
            .directory
            .ok_or_else(|| ActorError::InvalidConfiguration("directory is required".to_string()))?;

        // Create NodeId from listen address
        let node_id = NodeId::from_socket_addr(listen_addr);

        // Wrap directory in Rc<dyn Directory> as trait object for sharing
        let directory_rc: Rc<dyn crate::directory::Directory> = Rc::new(directory);

        // Create MessageBus with directory
        let message_bus = Rc::new(MessageBus::new(node_id.clone(), directory_rc.clone()));

        // Create network transport using foundation layer
        // ClientTransport for outgoing requests
        let peer_config = PeerConfig::default();
        let client_serializer = RequestResponseSerializer::new();
        let client_transport = ClientTransport::new(
            client_serializer,
            self.network_provider.clone(),
            self.time_provider.clone(),
            self.task_provider.clone(),
            peer_config,
        );

        let transport = FoundationTransport::new(client_transport);

        // Attach network transport to MessageBus
        message_bus.set_network_transport(Box::new(transport));

        // Create ServerTransport for incoming messages
        let server_serializer = RequestResponseSerializer::new();
        let listen_addr_str = listen_addr.to_string();
        let mut server_transport = ServerTransport::bind(
            self.network_provider,
            self.time_provider,
            self.task_provider.clone(),
            server_serializer,
            &listen_addr_str,
        )
        .await
        .map_err(|e| {
            ActorError::InvalidConfiguration(format!("Failed to bind ServerTransport: {}", e))
        })?;

        // Start event-driven message receive loop (NOT polling!)
        let message_bus_for_recv = message_bus.clone();
        self.task_provider
            .spawn_task("network_receive_loop", async move {
                loop {
                    // Wait for next incoming message (event-driven, blocks until message arrives)
                    if let Some(msg) = server_transport.next_message().await {
                        // Feed message into MessageBus for routing
                        let payload = msg.envelope.payload().to_vec();

                        // Deserialize and route the message
                        if let Ok(message) =
                            serde_json::from_slice::<crate::messaging::Message>(&payload)
                        {
                            tracing::info!(
                                "Received network message: target={}, method={}, direction={:?}, corr_id={}",
                                message.target_actor,
                                message.method_name,
                                message.direction,
                                message.correlation_id
                            );

                            // Route to local actor
                            if let Err(e) = message_bus_for_recv.route_message(message).await {
                                tracing::error!("Failed to route network message: {}", e);
                            } else {
                                tracing::info!("Successfully routed network message");
                            }

                            // Yield to scheduler to allow waiting tasks to run
                            // This is critical in LocalSet: after completing a oneshot channel,
                            // we must yield before polling next_message() to ensure the waiting
                            // task gets scheduled and can observe the completion
                            tokio::task::yield_now().await;
                        } else {
                            tracing::warn!("Failed to deserialize network message");
                        }
                    }
                }
            });

        tracing::info!(
            "ActorRuntime created: namespace={}, node_id={}, listen_addr={}, network=enabled",
            namespace,
            node_id,
            listen_addr
        );

        ActorRuntime::new(
            namespace,
            node_id,
            message_bus,
            directory_rc,
            self.task_provider,
        )
    }
}

impl Default for ActorRuntimeBuilder<(), (), ()> {
    fn default() -> Self {
        Self::new()
    }
}
