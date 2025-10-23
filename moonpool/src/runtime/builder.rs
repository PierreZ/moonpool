//! Actor runtime builder for configuration.

use crate::actor::NodeId;
use crate::directory::SimpleDirectory;
use crate::error::ActorError;
use crate::messaging::MessageBus;
use crate::runtime::ActorRuntime;
use moonpool_foundation::TokioTaskProvider;
use std::net::SocketAddr;
use std::rc::Rc;

/// Builder for ActorRuntime with fluent API.
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::ActorRuntime;
/// use moonpool::directory::SimpleDirectory;
///
/// // Create directory with cluster nodes
/// let nodes = vec![
///     NodeId::from("127.0.0.1:5000")?,
///     NodeId::from("127.0.0.1:5001")?,
/// ];
/// let directory = SimpleDirectory::new(nodes);
///
/// let runtime = ActorRuntime::builder()
///     .namespace("prod")
///     .listen_addr("127.0.0.1:5000")
///     .directory(directory)
///     .build()
///     .await?;
/// ```
pub struct ActorRuntimeBuilder {
    /// Cluster namespace (required).
    pub(crate) namespace: Option<String>,

    /// Listening address for incoming actor messages (required).
    pub(crate) listen_addr: Option<SocketAddr>,

    /// Directory for actor placement (required).
    pub(crate) directory: Option<SimpleDirectory>,
}

impl ActorRuntimeBuilder {
    /// Create a new runtime builder.
    pub fn new() -> Self {
        Self {
            namespace: None,
            listen_addr: None,
            directory: None,
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

    /// Build the ActorRuntime.
    ///
    /// Validates configuration and creates the runtime instance.
    ///
    /// # Errors
    ///
    /// Returns error if required fields are missing or invalid.
    pub async fn build(self) -> std::result::Result<ActorRuntime<TokioTaskProvider>, ActorError> {
        // Validate required fields
        let namespace = self
            .namespace
            .ok_or_else(|| ActorError::InvalidConfiguration("namespace is required".to_string()))?;

        let listen_addr = self.listen_addr.ok_or_else(|| {
            ActorError::InvalidConfiguration("listen_addr is required".to_string())
        })?;

        let directory = self.directory.ok_or_else(|| {
            ActorError::InvalidConfiguration("directory is required".to_string())
        })?;

        // Create NodeId from listen address
        let node_id = NodeId::from_socket_addr(listen_addr);

        // Create MessageBus with directory
        let message_bus = Rc::new(MessageBus::new(node_id.clone(), Rc::new(directory)));

        // Create TaskProvider (production uses Tokio)
        let task_provider = TokioTaskProvider;

        // TODO: Start listening for incoming connections
        // TODO: Wire MessageBus to PeerTransport (T069)

        tracing::info!(
            "ActorRuntime created: namespace={}, node_id={}, listen_addr={}",
            namespace,
            node_id,
            listen_addr
        );

        ActorRuntime::new(namespace, node_id, message_bus, task_provider)
    }
}

impl Default for ActorRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
