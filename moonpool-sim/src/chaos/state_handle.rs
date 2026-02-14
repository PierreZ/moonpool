//! Type-safe shared state for workload communication and invariant checking.
//!
//! `StateHandle` replaces the JSON-based `StateRegistry` with a type-safe
//! `Rc<RefCell<HashMap>>` backed store using `Box<dyn Any>`.
//!
//! # Usage
//!
//! ```ignore
//! use moonpool_sim::StateHandle;
//!
//! let state = StateHandle::new();
//! state.publish("counter", 42u64);
//! let val: Option<u64> = state.get("counter");
//! assert_eq!(val, Some(42));
//! ```

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Shared state handle for cross-workload communication and invariant checking.
///
/// Provides type-safe publish/get semantics using `std::any::Any` for type erasure.
/// Clone is cheap (Rc-based) — all clones share the same underlying storage.
#[derive(Clone, Default)]
pub struct StateHandle {
    inner: Rc<RefCell<HashMap<String, Box<dyn Any>>>>,
}

impl StateHandle {
    /// Create a new empty state handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a value under a key, replacing any existing value.
    pub fn publish<T: Any + 'static>(&self, key: &str, value: T) {
        self.inner
            .borrow_mut()
            .insert(key.to_string(), Box::new(value));
    }

    /// Get a cloned copy of the value under a key, if it exists and matches the type.
    ///
    /// Returns `None` if the key doesn't exist or the type doesn't match.
    pub fn get<T: Any + Clone + 'static>(&self, key: &str) -> Option<T> {
        self.inner
            .borrow()
            .get(key)
            .and_then(|v| v.downcast_ref::<T>())
            .cloned()
    }

    /// Check whether a key exists in the state.
    pub fn contains(&self, key: &str) -> bool {
        self.inner.borrow().contains_key(key)
    }
}

impl std::fmt::Debug for StateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.borrow().len();
        f.debug_struct("StateHandle")
            .field("key_count", &count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publish_and_get() {
        let state = StateHandle::new();
        state.publish("count", 42u64);
        assert_eq!(state.get::<u64>("count"), Some(42));
    }

    #[test]
    fn test_get_wrong_type_returns_none() {
        let state = StateHandle::new();
        state.publish("count", 42u64);
        assert_eq!(state.get::<String>("count"), None);
    }

    #[test]
    fn test_get_missing_key_returns_none() {
        let state = StateHandle::new();
        assert_eq!(state.get::<u64>("missing"), None);
    }

    #[test]
    fn test_contains() {
        let state = StateHandle::new();
        assert!(!state.contains("key"));
        state.publish("key", "value".to_string());
        assert!(state.contains("key"));
    }

    #[test]
    fn test_publish_replaces() {
        let state = StateHandle::new();
        state.publish("x", 1u64);
        state.publish("x", 2u64);
        assert_eq!(state.get::<u64>("x"), Some(2));
    }

    #[test]
    fn test_clone_shares_state() {
        let state = StateHandle::new();
        let clone = state.clone();
        state.publish("shared", true);
        assert_eq!(clone.get::<bool>("shared"), Some(true));
    }
}
