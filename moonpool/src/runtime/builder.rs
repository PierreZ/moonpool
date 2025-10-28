//! Actor runtime builder for configuration.

use crate::actor::NodeId;
use crate::error::ActorError;
use crate::runtime::ActorRuntime;
use moonpool_foundation::{NetworkProvider, TaskProvider, TimeProvider};
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
/// - `S`: Serializer (e.g., JsonSerializer, custom implementations)
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::ActorRuntime;
/// use moonpool::actor::NodeId;
/// use moonpool::serialization::JsonSerializer;
/// use moonpool_foundation::{TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider};
///
/// // Define cluster nodes
/// let nodes = vec![
///     NodeId::from("127.0.0.1:5000")?,
///     NodeId::from("127.0.0.1:5001")?,
/// ];
///
/// // Production configuration with Tokio providers
/// let runtime = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5000")
///     .cluster_nodes(nodes)
///     .with_providers(
///         TokioNetworkProvider,
///         TokioTimeProvider,
///         TokioTaskProvider,
///     )
///     .with_serializer(JsonSerializer)
///     .build()
///     .await?;
/// ```
pub struct ActorRuntimeBuilder<N, T, TP, S> {
    /// Cluster namespace (required).
    pub(crate) namespace: Option<String>,

    /// Listening address for incoming actor messages (required).
    pub(crate) listen_addr: Option<SocketAddr>,

    /// Cluster nodes for placement decisions (required).
    pub(crate) cluster_nodes: Option<Vec<NodeId>>,

    /// Shared directory for multi-node scenarios (optional).
    /// If None, a new directory will be created (single-node mode).
    pub(crate) shared_directory: Option<Rc<dyn crate::directory::Directory>>,

    /// Storage provider for actor state persistence (required).
    pub(crate) storage: Option<Rc<dyn crate::storage::StorageProvider>>,

    /// Message serializer for handler dispatch (required).
    pub(crate) serializer: Option<S>,

    /// Network provider (required).
    pub(crate) network_provider: N,

    /// Time provider (required).
    pub(crate) time_provider: T,

    /// Task provider (required).
    pub(crate) task_provider: TP,
}

impl ActorRuntimeBuilder<(), (), (), ()> {
    /// Create a new runtime builder.
    ///
    /// You must call `with_providers()` to set the network, time, and task providers
    /// before calling `build()`.
    pub fn new() -> Self {
        Self {
            namespace: None,
            listen_addr: None,
            cluster_nodes: None,
            shared_directory: None,
            storage: None,
            serializer: None,
            network_provider: (),
            time_provider: (),
            task_provider: (),
        }
    }
}

impl<N, T, TP, S> ActorRuntimeBuilder<N, T, TP, S> {
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
    ) -> ActorRuntimeBuilder<NewN, NewT, NewTP, S> {
        ActorRuntimeBuilder {
            namespace: self.namespace,
            listen_addr: self.listen_addr,
            cluster_nodes: self.cluster_nodes,
            shared_directory: self.shared_directory,
            storage: self.storage,
            serializer: self.serializer,
            network_provider,
            time_provider,
            task_provider,
        }
    }

    /// Set the message serializer for handler dispatch.
    ///
    /// Required before calling `build()`. This method consumes the builder and returns
    /// a new builder with the serializer set.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool::serialization::JsonSerializer;
    ///
    /// let runtime = ActorRuntime::builder()
    ///     .namespace("prod")
    ///     .listen_addr("127.0.0.1:5000")?
    ///     .with_serializer(JsonSerializer)
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_serializer<NewS: crate::serialization::Serializer>(
        self,
        serializer: NewS,
    ) -> ActorRuntimeBuilder<N, T, TP, NewS> {
        ActorRuntimeBuilder {
            namespace: self.namespace,
            listen_addr: self.listen_addr,
            cluster_nodes: self.cluster_nodes,
            shared_directory: self.shared_directory,
            storage: self.storage,
            serializer: Some(serializer),
            network_provider: self.network_provider,
            time_provider: self.time_provider,
            task_provider: self.task_provider,
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

    /// Set the cluster nodes for actor placement.
    ///
    /// These nodes are used by the placement strategy to decide where to
    /// activate new actors. The directory will track which actors are
    /// actually running on which nodes.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use moonpool::actor::NodeId;
    ///
    /// let nodes = vec![
    ///     NodeId::from("127.0.0.1:5000")?,
    ///     NodeId::from("127.0.0.1:5001")?,
    /// ];
    /// builder.cluster_nodes(nodes)
    /// ```
    pub fn cluster_nodes(mut self, nodes: Vec<NodeId>) -> Self {
        self.cluster_nodes = Some(nodes);
        self
    }

    /// Set a shared directory for multi-node scenarios.
    ///
    /// When running multiple nodes in the same process, you should create
    /// a single Directory instance and share it across all runtimes. This
    /// allows all nodes to see actor registrations from other nodes.
    ///
    /// If not provided, each runtime will create its own directory instance
    /// (only suitable for single-node scenarios).
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use moonpool::directory::SimpleDirectory;
    /// use std::rc::Rc;
    ///
    /// // Create shared directory once
    /// let shared_directory = Rc::new(SimpleDirectory::new()) as Rc<dyn Directory>;
    ///
    /// // Pass to all runtimes
    /// let runtime1 = ActorRuntime::builder()
    ///     .namespace("prod")
    ///     .listen_addr("127.0.0.1:5000")?
    ///     .cluster_nodes(nodes.clone())
    ///     .shared_directory(shared_directory.clone())
    ///     .with_providers(...)
    ///     .build()
    ///     .await?;
    ///
    /// let runtime2 = ActorRuntime::builder()
    ///     .namespace("prod")
    ///     .listen_addr("127.0.0.1:5001")?
    ///     .cluster_nodes(nodes.clone())
    ///     .shared_directory(shared_directory.clone())
    ///     .with_providers(...)
    ///     .build()
    ///     .await?;
    /// ```
    pub fn shared_directory(mut self, directory: Rc<dyn crate::directory::Directory>) -> Self {
        self.shared_directory = Some(directory);
        self
    }

    /// Set the storage provider for actor state persistence.
    ///
    /// Required for actors with persistent state. All actors in this runtime
    /// will use this storage provider.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use moonpool::storage::InMemoryStorage;
    /// use std::rc::Rc;
    ///
    /// let storage = Rc::new(InMemoryStorage::new()) as Rc<dyn StorageProvider>;
    /// builder.with_storage(storage)
    /// ```
    pub fn with_storage(mut self, storage: Rc<dyn crate::storage::StorageProvider>) -> Self {
        self.storage = Some(storage);
        self
    }
}

impl<N, T, TP, S> ActorRuntimeBuilder<N, T, TP, S>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    S: crate::serialization::Serializer + Clone + 'static,
{
    /// Build the ActorRuntime.
    ///
    /// Validates configuration and creates the runtime instance by calling `ActorRuntime::new()`.
    ///
    /// # Errors
    ///
    /// Returns error if required fields are missing or invalid.
    pub async fn build(self) -> std::result::Result<ActorRuntime<TP, S>, ActorError> {
        // Validate required fields
        let namespace = self
            .namespace
            .ok_or_else(|| ActorError::InvalidConfiguration("namespace is required".to_string()))?;

        let listen_addr = self.listen_addr.ok_or_else(|| {
            ActorError::InvalidConfiguration("listen_addr is required".to_string())
        })?;

        let cluster_nodes = self.cluster_nodes.ok_or_else(|| {
            ActorError::InvalidConfiguration("cluster_nodes is required".to_string())
        })?;

        let storage = self
            .storage
            .ok_or_else(|| ActorError::InvalidConfiguration("storage is required".to_string()))?;

        let serializer = self.serializer.ok_or_else(|| {
            ActorError::InvalidConfiguration(
                "serializer is required (use .with_serializer())".to_string(),
            )
        })?;

        // Delegate to ActorRuntime::new() with all the builder's parameters
        ActorRuntime::new(
            namespace,
            listen_addr.to_string(),
            cluster_nodes,
            self.shared_directory,
            storage,
            self.network_provider,
            self.time_provider,
            self.task_provider,
            serializer,
        )
        .await
    }
}

impl Default for ActorRuntimeBuilder<(), (), (), ()> {
    fn default() -> Self {
        Self::new()
    }
}
