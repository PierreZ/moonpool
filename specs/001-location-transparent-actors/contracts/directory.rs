// API Contract: Directory Service
// This is a specification file, not compilable code

/// Distributed directory for actor location tracking.
///
/// Maps `ActorId` to `NodeId` with eventual consistency guarantees.
#[async_trait(?Send)]
pub trait Directory {
    /// Lookup actor's hosting node.
    ///
    /// ## Returns
    /// - `Some(node_id)` if actor is currently activated
    /// - `None` if actor not activated or lookup failed
    ///
    /// ## Caching
    /// Implementations may cache lookups locally.
    /// Cache invalidation handled via message headers (Orleans pattern).
    ///
    /// ## Example
    /// ```ignore
    /// let node = directory.lookup(&actor_id).await?;
    /// if let Some(node_id) = node {
    ///     // Send message to node_id
    /// } else {
    ///     // Trigger activation via placement algorithm
    /// }
    /// ```
    async fn lookup(&self, actor_id: &ActorId) -> Option<NodeId>;

    /// Register actor activation on node.
    ///
    /// ## Concurrency
    /// If multiple nodes attempt concurrent registration for same actor_id,
    /// directory MUST serialize and return placement decision.
    ///
    /// ## Interior Mutability
    /// Uses `&self` for Arc compatibility. Implementations use RefCell/RwLock internally.
    ///
    /// ## Returns
    /// - `PlaceOnNode(node_id)` - Registration succeeded, activate on this node
    /// - `AlreadyRegistered(node_id)` - Actor already active elsewhere, forward message
    /// - `Race { winner, loser }` - Concurrent activation detected, loser must deactivate
    ///
    /// ## Example
    /// ```ignore
    /// match directory.register(actor_id, my_node_id).await? {
    ///     PlacementDecision::PlaceOnNode(node) => {
    ///         assert_eq!(node, my_node_id);
    ///         // Continue activation
    ///     }
    ///     PlacementDecision::AlreadyRegistered(node) => {
    ///         // Forward message to existing activation
    ///     }
    ///     PlacementDecision::Race { winner, loser } => {
    ///         if winner == my_node_id {
    ///             // We won, continue activation
    ///         } else {
    ///             // We lost, deactivate and forward
    ///         }
    ///     }
    /// }
    /// ```
    async fn register(
        &self,
        actor_id: ActorId,
        node_id: NodeId,
    ) -> Result<PlacementDecision, DirectoryError>;

    /// Unregister actor from directory.
    ///
    /// ## Behavior
    /// - Removes actor_id → node_id mapping
    /// - Idempotent: no error if not registered
    /// - Invalidates local caches
    ///
    /// ## Interior Mutability
    /// Uses `&self` for Arc compatibility. Implementations use RefCell/RwLock internally.
    ///
    /// ## Example
    /// ```ignore
    /// directory.unregister(&actor_id).await?;
    /// ```
    async fn unregister(&self, actor_id: &ActorId) -> Result<(), DirectoryError>;

    /// Get current load for node (for placement algorithm).
    ///
    /// ## Returns
    /// Number of active actors on specified node.
    ///
    /// Used by two-random-choices placement algorithm.
    async fn get_node_load(&self, node_id: NodeId) -> usize;
}

/// Placement decision from directory registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementDecision {
    /// Actor successfully placed on node.
    ///
    /// Caller should continue activation.
    PlaceOnNode(NodeId),

    /// Actor already registered on another node.
    ///
    /// Caller should forward message to existing activation.
    AlreadyRegistered(NodeId),

    /// Concurrent activation race detected.
    ///
    /// Winner continues activation, loser deactivates and forwards.
    Race { winner: NodeId, loser: NodeId },
}

/// Directory errors.
#[derive(Debug, Clone)]
pub enum DirectoryError {
    /// Directory operation timed out.
    Timeout,

    /// Network error communicating with directory.
    NetworkError(String),

    /// Invalid actor ID format.
    InvalidActorId(ActorId),

    /// Invalid node ID (not in cluster).
    InvalidNodeId(NodeId),
}

/// Simple in-memory directory implementation.
///
/// Provides eventual consistency with local caching.
/// Suitable for static cluster topology (no node joins/leaves).
///
/// ## Single-Threaded Pattern
/// Uses RefCell for interior mutability (current_thread runtime, no Send/Sync needed).
pub struct SimpleDirectory {
    /// Authoritative mapping: ActorId → NodeId
    entries: RefCell<HashMap<ActorId, NodeId>>,

    /// Local cache: ActorId → (NodeId, cached_at)
    local_cache: RefCell<HashMap<ActorId, (NodeId, Instant)>>,

    /// Node load tracking: NodeId → actor count
    node_load: RefCell<HashMap<NodeId, usize>>,

    /// List of all nodes in cluster (for placement algorithm)
    cluster_nodes: Vec<NodeId>,

    /// Random number generator for placement (single-threaded, RefCell is sufficient)
    rng: RefCell<Box<dyn RngCore>>,
}

impl SimpleDirectory {
    /// Create directory for cluster.
    ///
    /// ## Parameters
    /// - `cluster_nodes` - All node IDs in static cluster
    ///
    /// ## Example
    /// ```ignore
    /// let directory = SimpleDirectory::new(vec![
    ///     NodeId::from("127.0.0.1:5000")?,
    ///     NodeId::from("127.0.0.1:5001")?,
    ///     NodeId::from("127.0.0.1:5002")?,
    /// ]);
    /// ```
    pub fn new(cluster_nodes: Vec<NodeId>) -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
            local_cache: RefCell::new(HashMap::new()),
            node_load: RefCell::new(
                cluster_nodes.iter().map(|&id| (id, 0)).collect()
            ),
            cluster_nodes,
            rng: RefCell::new(Box::new(rand::thread_rng())),
        }
    }

    /// Two-random-choices placement algorithm.
    ///
    /// ## Algorithm
    /// 1. Randomly select two nodes from cluster
    /// 2. Return node with lower current load
    /// 3. Breaks ties randomly
    ///
    /// ## Guarantees
    /// - Load balances better than pure random (O(log log N) imbalance)
    /// - No coordination required (embarrassingly parallel)
    /// - Deterministic given same RNG seed (simulation testing)
    ///
    /// ## Example
    /// ```ignore
    /// let chosen_node = directory.choose_node_for_placement();
    /// ```
    fn choose_node_for_placement(&self) -> NodeId {
        let node_load = self.node_load.borrow();

        // Pick two random nodes (single-threaded, RefCell borrow_mut)
        let mut rng = self.rng.borrow_mut();
        let idx1 = rng.gen_range(0..self.cluster_nodes.len());
        let idx2 = rng.gen_range(0..self.cluster_nodes.len());

        let node1 = self.cluster_nodes[idx1];
        let node2 = self.cluster_nodes[idx2];

        let load1 = node_load.get(&node1).copied().unwrap_or(0);
        let load2 = node_load.get(&node2).copied().unwrap_or(0);

        if load1 <= load2 {
            node1
        } else {
            node2
        }
    }

    /// Invalidate local cache entry.
    ///
    /// Called when stale address detected (message forwarded).
    pub fn invalidate_cache(&self, actor_id: &ActorId) {
        self.local_cache.borrow_mut().remove(actor_id);
    }

    /// Update local cache with known-good address.
    ///
    /// Called when message successfully delivered or cache invalidation header received.
    pub fn update_cache(&self, actor_id: ActorId, node_id: NodeId) {
        self.local_cache
            .borrow_mut()
            .insert(actor_id, (node_id, Instant::now()));
    }
}

#[async_trait(?Send)]
impl Directory for SimpleDirectory {
    async fn lookup(&self, actor_id: &ActorId) -> Option<NodeId> {
        // Check local cache first (fast path)
        if let Some((node_id, _cached_at)) = self.local_cache.borrow().get(actor_id) {
            return Some(*node_id);
        }

        // Query authoritative registry
        let node_id = self.entries.borrow().get(actor_id).copied();

        // Update cache if found
        if let Some(node_id) = node_id {
            self.update_cache(actor_id.clone(), node_id);
        }

        node_id
    }

    async fn register(
        &self,
        actor_id: ActorId,
        node_id: NodeId,
    ) -> Result<PlacementDecision, DirectoryError> {
        let mut entries = self.entries.borrow_mut();

        // Check if already registered
        if let Some(&existing_node) = entries.get(&actor_id) {
            if existing_node == node_id {
                // Idempotent: already registered on this node
                return Ok(PlacementDecision::PlaceOnNode(node_id));
            } else {
                // Concurrent activation race: first registration wins
                return Ok(PlacementDecision::Race {
                    winner: existing_node,
                    loser: node_id,
                });
            }
        }

        // First registration for this actor
        entries.insert(actor_id.clone(), node_id);

        // Update load tracking
        let mut node_load = self.node_load.borrow_mut();
        *node_load.entry(node_id).or_insert(0) += 1;

        // Update local cache
        self.update_cache(actor_id, node_id);

        Ok(PlacementDecision::PlaceOnNode(node_id))
    }

    async fn unregister(&self, actor_id: &ActorId) -> Result<(), DirectoryError> {
        let mut entries = self.entries.borrow_mut();

        if let Some(node_id) = entries.remove(actor_id) {
            // Decrement load tracking
            let mut node_load = self.node_load.borrow_mut();
            if let Some(load) = node_load.get_mut(&node_id) {
                *load = load.saturating_sub(1);
            }

            // Invalidate cache
            self.invalidate_cache(actor_id);
        }

        Ok(())
    }

    async fn get_node_load(&self, node_id: NodeId) -> usize {
        self.node_load
            .borrow()
            .get(&node_id)
            .copied()
            .unwrap_or(0)
    }
}

// Note: CacheUpdate and ActorAddress are defined in message.rs
// They are part of the message protocol for cache invalidation.

/// Example usage in message forwarding.
///
/// This demonstrates how cache invalidation integrates with message routing.
pub struct MessageForwarder;

impl MessageForwarder {
    /// Forward message when actor not found on expected node.
    ///
    /// ## Behavior
    /// 1. Look up current actor location
    /// 2. Forward message to new node
    /// 3. Attach cache invalidation header to response
    ///
    /// ## Example
    /// ```ignore
    /// async fn forward_message(
    ///     message: Message,
    ///     stale_node: NodeId,
    ///     directory: &dyn Directory,
    /// ) -> Result<Message, ForwardError> {
    ///     // Look up current location
    ///     let current_node = directory.lookup(&message.target_actor).await
    ///         .ok_or(ForwardError::ActorNotFound)?;
    ///
    ///     // Forward to current location
    ///     let response = send_to_node(current_node, message).await?;
    ///
    ///     // Attach cache invalidation
    ///     let stale_addr = ActorAddress {
    ///         actor_id: message.target_actor.clone(),
    ///         node_id: stale_node,
    ///         activation_time: Instant::now(),  // Unknown, use current
    ///     };
    ///     let valid_addr = ActorAddress {
    ///         actor_id: message.target_actor.clone(),
    ///         node_id: current_node,
    ///         activation_time: Instant::now(),
    ///     };
    ///
    ///     response.cache_invalidation = Some(vec![
    ///         CacheUpdate::new(stale_addr, Some(valid_addr))
    ///     ]);
    ///
    ///     Ok(response)
    /// }
    /// ```
    pub async fn forward_message(
        message: Message,
        stale_node: NodeId,
        directory: &dyn Directory,
    ) -> Result<Message, DirectoryError> {
        unimplemented!("see messaging/bus.rs")
    }
}

// Note: This directory implementation is suitable for static clusters with
// eventual consistency. For production with dynamic membership or strong
// consistency requirements, consider:
// - Raft/Paxos consensus for strong consistency
// - Version vectors or CRDTs for conflict resolution
// - Gossip protocol for membership changes
//
// These are out of scope for initial implementation per spec.md.
