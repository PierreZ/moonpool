//! Actor state persistence: store and typed persistent state.

pub(crate) mod persistent;
pub(crate) mod store;

pub use persistent::PersistentState;
pub use store::{ActorStateError, ActorStateStore, InMemoryStateStore, StoredState};
