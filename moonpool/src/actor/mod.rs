//! Actor lifecycle and core types.

pub mod catalog;
pub mod context;
pub mod extensions;
pub mod factory;
pub mod handlers;
pub mod id;
pub mod lifecycle;
pub mod reference;
pub mod state;
pub mod traits;

// Re-exports (only what exists now)
pub use catalog::{ActivationDirectory, ActorCatalog};
pub use context::{ActorContext, LifecycleCommand, run_message_loop};
pub use factory::ActorFactory;
pub use handlers::HandlerRegistry;
pub use id::{ActorId, CorrelationId, NodeId};
pub use lifecycle::{ActivationState, DeactivationReason};
pub use reference::ActorRef;
pub use state::ActorState;
pub use traits::{Actor, MessageHandler};

/// Placement hint for actor activation.
///
/// Actors can provide hints to influence where they are activated in the cluster.
/// The directory uses these hints to make placement decisions for new activations.
///
/// # Variants
///
/// - **Random**: Activate on a random node (uniform distribution)
/// - **Local**: Prefer activation on the local node (caller's node)
/// - **LeastLoaded**: Activate on the node with fewest active actors (load balancing)
///
/// # Usage
///
/// Override the `placement_hint()` method in your Actor implementation:
///
/// ```rust,ignore
/// use moonpool::prelude::*;
///
/// #[async_trait(?Send)]
/// impl Actor for MyActor {
///     type State = ();
///     const ACTOR_TYPE: &'static str = "MyActor";
///
///     fn placement_hint() -> PlacementHint {
///         PlacementHint::LeastLoaded  // Load-balanced placement
///     }
///
///     // ... other methods
/// }
/// ```
///
/// # When to Use Local
///
/// Use `PlacementHint::Local` for:
/// - Actors with high message throughput (reduces network overhead)
/// - Short-lived actors (avoid remote activation cost)
/// - Actors with strong locality (e.g., session actors)
///
/// # When to Use Random
///
/// Use `PlacementHint::Random` for:
/// - Simple stateless distribution
/// - When load balancing is not critical
/// - Testing and development
///
/// # When to Use LeastLoaded
///
/// Use `PlacementHint::LeastLoaded` for:
/// - Long-lived actors that need even distribution (default recommendation)
/// - Heterogeneous workloads with varying actor lifetimes
/// - Production deployments requiring automatic load balancing
/// - Scenarios where some nodes may be overloaded
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum PlacementHint {
    /// Activate on a random node (uniform distribution).
    ///
    /// The directory will choose a node using simple random selection.
    /// This provides basic distribution but doesn't account for current load.
    #[default]
    Random,

    /// Prefer activation on the local node.
    ///
    /// The directory will attempt to place the actor on the same node
    /// as the caller. This minimizes network overhead for high-throughput
    /// actors or actors with strong locality requirements.
    Local,

    /// Activate on the node with the fewest active actors (load balancing).
    ///
    /// The directory will choose the node with the lowest activation count.
    /// This provides automatic load balancing based on current cluster state.
    /// Recommended as the default for most production deployments.
    LeastLoaded,
}

