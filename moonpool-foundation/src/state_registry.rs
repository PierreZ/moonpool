//! Simple state registry for simulation invariant checking.
//!
//! This module provides infrastructure for actors to expose their internal state
//! in a structured format (JSON). The runner collects all actor states after every
//! simulation event and passes them to invariant check functions.
//!
//! # Philosophy
//!
//! Similar to DTrace probes, this provides zero-overhead observability hooks:
//! - Actors control what state to expose
//! - Updates happen at meaningful points (actor decides when)
//! - No overhead when invariants are not registered
//! - Foundation for DTrace-style debugging tools
//!
//! # Example
//!
//! ```rust
//! use moonpool_foundation::StateRegistry;
//! use serde_json::json;
//!
//! let registry = StateRegistry::new();
//!
//! // Actor registers its state
//! registry.register_state("client_1", json!({
//!     "messages_sent": 42,
//!     "connected": true,
//! }));
//!
//! // Runner collects all states for invariant checking
//! let all_states = registry.get_all_states();
//! ```

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Registry for actor state during simulation.
///
/// Actors write their state as JSON to this registry. The runner
/// reads all states after every simulation event to check invariants.
///
/// # Thread Safety
///
/// Uses `Arc<Mutex<HashMap>>` for interior mutability. In single-threaded
/// simulation context, the Mutex has no contention overhead.
#[derive(Clone, Default, Debug)]
pub struct StateRegistry {
    states: Arc<Mutex<HashMap<String, Value>>>,
}

impl StateRegistry {
    /// Create a new empty state registry.
    pub fn new() -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register or update state for a named actor.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique identifier for this actor (typically IP address or actor name)
    /// * `state` - JSON representation of actor's current state
    ///
    /// # Example
    ///
    /// ```rust
    /// use serde_json::json;
    /// # use moonpool_foundation::StateRegistry;
    ///
    /// # let registry = StateRegistry::new();
    /// registry.register_state("10.0.0.1", json!({
    ///     "messages_sent": 42,
    ///     "timeouts": 3,
    ///     "connected": true,
    /// }));
    /// ```
    pub fn register_state(&self, name: impl Into<String>, state: Value) {
        self.states
            .lock()
            .expect("Failed to lock state registry")
            .insert(name.into(), state);
    }

    /// Get current state for a named actor.
    ///
    /// Returns `None` if the actor hasn't registered state yet.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use moonpool_foundation::StateRegistry;
    /// # let registry = StateRegistry::new();
    /// # use serde_json::json;
    /// # registry.register_state("actor_1", json!({"count": 1}));
    ///
    /// if let Some(state) = registry.get_state("actor_1") {
    ///     let count = state.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
    ///     println!("Actor has count: {}", count);
    /// }
    /// ```
    pub fn get_state(&self, name: &str) -> Option<Value> {
        self.states
            .lock()
            .expect("Failed to lock state registry")
            .get(name)
            .cloned()
    }

    /// Get all registered actor states.
    ///
    /// Returns a snapshot of all actor states at this moment.
    /// Used by the runner to collect states for invariant checking.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use moonpool_foundation::StateRegistry;
    /// # use serde_json::json;
    /// # let registry = StateRegistry::new();
    /// # registry.register_state("actor_1", json!({"count": 1}));
    /// # registry.register_state("actor_2", json!({"count": 2}));
    ///
    /// let all_states = registry.get_all_states();
    /// for (name, state) in all_states {
    ///     println!("{}: {:?}", name, state);
    /// }
    /// ```
    pub fn get_all_states(&self) -> HashMap<String, Value> {
        self.states
            .lock()
            .expect("Failed to lock state registry")
            .clone()
    }

    /// Clear all registered states.
    ///
    /// Called between simulation iterations to start fresh.
    /// Not typically needed by users - the runner handles this.
    pub fn clear(&self) {
        self.states
            .lock()
            .expect("Failed to lock state registry")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_register_and_get_state() {
        let registry = StateRegistry::new();

        registry.register_state("actor_1", json!({"count": 42}));

        let state = registry.get_state("actor_1").unwrap();
        assert_eq!(state.get("count").and_then(|c| c.as_u64()), Some(42));
    }

    #[test]
    fn test_get_nonexistent_state() {
        let registry = StateRegistry::new();
        assert!(registry.get_state("nonexistent").is_none());
    }

    #[test]
    fn test_update_existing_state() {
        let registry = StateRegistry::new();

        registry.register_state("actor_1", json!({"count": 1}));
        registry.register_state("actor_1", json!({"count": 2}));

        let state = registry.get_state("actor_1").unwrap();
        assert_eq!(state.get("count").and_then(|c| c.as_u64()), Some(2));
    }

    #[test]
    fn test_get_all_states() {
        let registry = StateRegistry::new();

        registry.register_state("actor_1", json!({"count": 1}));
        registry.register_state("actor_2", json!({"count": 2}));
        registry.register_state("actor_3", json!({"count": 3}));

        let all_states = registry.get_all_states();
        assert_eq!(all_states.len(), 3);
        assert!(all_states.contains_key("actor_1"));
        assert!(all_states.contains_key("actor_2"));
        assert!(all_states.contains_key("actor_3"));
    }

    #[test]
    fn test_clear() {
        let registry = StateRegistry::new();

        registry.register_state("actor_1", json!({"count": 1}));
        registry.register_state("actor_2", json!({"count": 2}));

        assert_eq!(registry.get_all_states().len(), 2);

        registry.clear();

        assert_eq!(registry.get_all_states().len(), 0);
        assert!(registry.get_state("actor_1").is_none());
    }

    #[test]
    fn test_clone_shares_state() {
        let registry1 = StateRegistry::new();
        let registry2 = registry1.clone();

        registry1.register_state("actor_1", json!({"count": 1}));

        // Cloned registry sees the same state
        let state = registry2.get_state("actor_1").unwrap();
        assert_eq!(state.get("count").and_then(|c| c.as_u64()), Some(1));

        // Updates from clone visible in original
        registry2.register_state("actor_2", json!({"count": 2}));
        assert!(registry1.get_state("actor_2").is_some());
    }
}
