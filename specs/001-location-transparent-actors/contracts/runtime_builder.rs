// API Contract: ActorRuntime Builder
// This is a specification file, not compilable code

use moonpool_foundation::network::NetworkProvider;
use moonpool_foundation::time::TimeProvider;
use moonpool_foundation::task::TaskProvider;

/// Builder for configuring ActorRuntime with provider injection.
///
/// ## Required Fields
/// - `namespace` - Cluster namespace (e.g., "prod", "staging", "tenant-acme")
/// - `listen_addr` - Network address for this node (e.g., "127.0.0.1:5000")
///
/// ## Optional Fields (with defaults)
/// - `directory` - Shared directory for actor location (default: creates new SimpleDirectory)
/// - `storage` - Shared storage for actor state (default: creates new InMemoryStorage)
/// - `network` - Network provider for TCP operations (default: TokioNetworkProvider)
/// - `time` - Time provider for sleep/timeout (default: TokioTimeProvider)
/// - `task` - Task provider for spawning tasks (default: TokioTaskProvider)
///
/// ## Example: Production (Single Node)
/// ```ignore
/// use moonpool::ActorRuntime;
///
/// let runtime = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5000")
///     .build()
///     .await?;
/// ```
///
/// ## Example: Production (Multi-Node Cluster)
/// ```ignore
/// use moonpool::ActorRuntime;
/// use moonpool::directory::SimpleDirectory;
/// use moonpool::storage::InMemoryStorage;
///
/// // Create shared infrastructure
/// let directory = Arc::new(SimpleDirectory::new());
/// let storage = Arc::new(InMemoryStorage::new());
///
/// // Node 1
/// let node1 = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5000")
///     .directory(directory.clone())
///     .storage(storage.clone())
///     .build()
///     .await?;
///
/// // Node 2 (shares same directory and storage)
/// let node2 = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5001")
///     .directory(directory.clone())
///     .storage(storage.clone())
///     .build()
///     .await?;
/// ```
///
/// ## Example: Simulation Testing
/// ```ignore
/// use moonpool::ActorRuntime;
/// use moonpool::directory::SimpleDirectory;
/// use moonpool::storage::InMemoryStorage;
/// use moonpool_foundation::{SimWorld, SimNetworkProvider, SimTimeProvider, TokioTaskProvider};
///
/// let sim = SimWorld::new();
/// let directory = Arc::new(SimpleDirectory::new());
/// let storage = Arc::new(InMemoryStorage::new());
///
/// // Create simulation providers
/// let network = SimNetworkProvider::new(sim.downgrade());
/// let time = SimTimeProvider::new(sim.downgrade());
/// let task = TokioTaskProvider::new();
///
/// // Node 1 with simulation providers
/// let node1 = ActorRuntime::builder()
///     .namespace("test")
///     .listen_addr("127.0.0.1:5000")
///     .directory(directory.clone())
///     .storage(storage.clone())
///     .network(network.clone())
///     .time(time.clone())
///     .task(task.clone())
///     .build()
///     .await?;
///
/// // Node 2 with same providers
/// let node2 = ActorRuntime::builder()
///     .namespace("test")
///     .listen_addr("127.0.0.1:5001")
///     .directory(directory.clone())
///     .storage(storage.clone())
///     .network(network.clone())
///     .time(time.clone())
///     .task(task.clone())
///     .build()
///     .await?;
///
/// // Run simulation with chaos testing
/// sim.run().await?;
/// ```
pub struct ActorRuntimeBuilder<N, T, TP>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    namespace: Option<String>,
    listen_addr: Option<String>,
    directory: Option<Arc<dyn Directory>>,
    storage: Option<Arc<dyn StorageProvider>>,
    network: Option<N>,
    time: Option<T>,
    task: Option<TP>,
}

impl ActorRuntimeBuilder<TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider> {
    /// Create new builder with default production providers.
    ///
    /// Uses Tokio-based providers for real networking, time, and task spawning.
    pub fn new() -> Self {
        Self {
            namespace: None,
            listen_addr: None,
            directory: None,
            storage: None,
            network: None,
            time: None,
            task: None,
        }
    }
}

impl<N, T, TP> ActorRuntimeBuilder<N, T, TP>
where
    N: NetworkProvider,
    T: TimeProvider,
    TP: TaskProvider,
{
    /// Set cluster namespace (required).
    ///
    /// ## Use Cases
    /// - **Environment isolation**: "prod", "staging", "dev"
    /// - **Multi-tenancy**: "tenant-{id}"
    /// - **Testing**: "test-{run-id}"
    ///
    /// ## Example
    /// ```ignore
    /// builder.namespace("prod")
    /// ```
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// Set listening address for this node (required).
    ///
    /// ## Format
    /// - IPv4: "127.0.0.1:5000"
    /// - IPv6: "[::1]:5000"
    /// - Hostname: "node1.cluster:5000"
    ///
    /// ## Example
    /// ```ignore
    /// builder.listen_addr("127.0.0.1:5000")
    /// ```
    pub fn listen_addr(mut self, addr: impl Into<String>) -> Self {
        self.listen_addr = Some(addr.into());
        self
    }

    /// Set shared directory for actor location (optional).
    ///
    /// If not provided, creates a new SimpleDirectory instance.
    /// For multi-node clusters, pass the same directory instance to all nodes.
    ///
    /// ## Example
    /// ```ignore
    /// let directory = Arc::new(SimpleDirectory::new());
    /// builder.directory(directory.clone())
    /// ```
    pub fn directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Set shared storage for actor state (optional).
    ///
    /// If not provided, creates a new InMemoryStorage instance.
    /// For multi-node clusters, pass the same storage instance to all nodes.
    ///
    /// ## Example
    /// ```ignore
    /// let storage = Arc::new(InMemoryStorage::new());
    /// builder.storage(storage.clone())
    /// ```
    pub fn storage(mut self, storage: Arc<dyn StorageProvider>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Set network provider for TCP operations (optional).
    ///
    /// If not provided, uses TokioNetworkProvider for production.
    /// For simulation testing, use SimNetworkProvider.
    ///
    /// ## Example (Simulation)
    /// ```ignore
    /// let sim = SimWorld::new();
    /// let network = SimNetworkProvider::new(sim.downgrade());
    /// builder.network(network)
    /// ```
    pub fn network<N2>(self, network: N2) -> ActorRuntimeBuilder<N2, T, TP>
    where
        N2: NetworkProvider,
    {
        ActorRuntimeBuilder {
            namespace: self.namespace,
            listen_addr: self.listen_addr,
            directory: self.directory,
            storage: self.storage,
            network: Some(network),
            time: self.time,
            task: self.task,
        }
    }

    /// Set time provider for sleep/timeout operations (optional).
    ///
    /// If not provided, uses TokioTimeProvider for production.
    /// For simulation testing, use SimTimeProvider.
    ///
    /// ## Example (Simulation)
    /// ```ignore
    /// let sim = SimWorld::new();
    /// let time = SimTimeProvider::new(sim.downgrade());
    /// builder.time(time)
    /// ```
    pub fn time<T2>(self, time: T2) -> ActorRuntimeBuilder<N, T2, TP>
    where
        T2: TimeProvider,
    {
        ActorRuntimeBuilder {
            namespace: self.namespace,
            listen_addr: self.listen_addr,
            directory: self.directory,
            storage: self.storage,
            network: self.network,
            time: Some(time),
            task: self.task,
        }
    }

    /// Set task provider for spawning tasks (optional).
    ///
    /// If not provided, uses TokioTaskProvider.
    ///
    /// ## Example
    /// ```ignore
    /// let task = TokioTaskProvider::new();
    /// builder.task(task)
    /// ```
    pub fn task<TP2>(self, task: TP2) -> ActorRuntimeBuilder<N, T, TP2>
    where
        TP2: TaskProvider,
    {
        ActorRuntimeBuilder {
            namespace: self.namespace,
            listen_addr: self.listen_addr,
            directory: self.directory,
            storage: self.storage,
            network: self.network,
            time: self.time,
            task: Some(task),
        }
    }

    /// Build the ActorRuntime.
    ///
    /// ## Errors
    /// - Returns error if required fields (namespace, listen_addr) not set
    /// - Returns error if listen_addr cannot be parsed or bound
    ///
    /// ## Example
    /// ```ignore
    /// let runtime = ActorRuntime::builder()
    ///     .namespace("prod")
    ///     .listen_addr("127.0.0.1:5000")
    ///     .build()
    ///     .await?;
    /// ```
    pub async fn build(self) -> Result<ActorRuntime, ActorError> {
        let namespace = self.namespace.ok_or(ActorError::MissingConfiguration("namespace"))?;
        let listen_addr = self.listen_addr.ok_or(ActorError::MissingConfiguration("listen_addr"))?;

        // Create or use provided directory
        let directory = self.directory.unwrap_or_else(|| Arc::new(SimpleDirectory::new()));

        // Create or use provided storage
        let storage = self.storage;

        // Create or use provided network provider (default: Tokio)
        let network = self.network.unwrap_or_else(|| N::default());

        // Create or use provided time provider (default: Tokio)
        let time = self.time.unwrap_or_else(|| T::default());

        // Create or use provided task provider (default: Tokio)
        let task = self.task.unwrap_or_else(|| TP::default());

        // Parse listen address to NodeId
        let node_id = NodeId::from(listen_addr.clone())?;

        // Bind network listener using network provider
        let listener = network.bind(&listen_addr).await?;

        // TODO: Initialize MessageBus, ActorCatalog with providers
        // MessageBus will use:
        // - network: for connecting to other nodes
        // - time: for timeouts and retry delays
        // - task: for spawning message processing tasks
        unimplemented!("see runtime/mod.rs")
    }
}
