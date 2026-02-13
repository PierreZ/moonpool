//! Shared state handle for cross-workload state sharing.
//!
//! `StateHandle` is a cheap-to-clone handle wrapping a `Rc<RefCell<HashMap>>`
//! that workloads use to publish and read typed state. Invariants read from
//! the same handle to validate cross-workload properties.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A shared, cloneable handle for publishing and reading typed state.
///
/// All workloads and invariants in a simulation share the same underlying
/// state map. Values are type-erased via `Box<dyn Any>`.
///
/// # Example
///
/// ```ignore
/// let state = StateHandle::new();
/// state.publish("counter", 42u64);
/// assert_eq!(state.get::<u64>("counter"), Some(42u64));
/// ```
#[derive(Clone)]
pub struct StateHandle {
    inner: Rc<RefCell<HashMap<String, Box<dyn Any>>>>,
}

impl StateHandle {
    /// Create a new empty state handle.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Publish a typed value under the given key.
    ///
    /// If a value already exists for this key, it is replaced.
    pub fn publish<T: Any + 'static>(&self, key: &str, value: T) {
        self.inner
            .borrow_mut()
            .insert(key.to_string(), Box::new(value));
    }

    /// Get a clone of the typed value stored under the given key.
    ///
    /// Returns `None` if the key doesn't exist or the stored value
    /// doesn't match type `T`.
    pub fn get<T: Any + Clone>(&self, key: &str) -> Option<T> {
        self.inner
            .borrow()
            .get(key)
            .and_then(|v| v.downcast_ref::<T>())
            .cloned()
    }

    /// Check whether a value exists for the given key.
    pub fn contains(&self, key: &str) -> bool {
        self.inner.borrow().contains_key(key)
    }
}

impl Default for StateHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for StateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<_> = self.inner.borrow().keys().cloned().collect();
        f.debug_struct("StateHandle").field("keys", &keys).finish()
    }
}
