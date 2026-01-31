//! Core simulation engine for deterministic testing.
//!
//! This module provides the central SimWorld coordinator that manages time,
//! event processing, and network simulation state.
//!
//! ## Submodules
//!
//! - `world` - Core SimWorld and WeakSimWorld types
//! - `events` - Event types and queue for scheduling
//! - `state` - Network and storage state management
//! - `wakers` - Waker management for async coordination
//! - `sleep` - Sleep future for simulation time
//! - `rng` - Thread-local random number generation

pub mod events;
pub mod rng;
pub mod sleep;
pub mod state;
pub mod storage_ops;
pub mod wakers;
pub mod world;

// Re-export main types at module level
pub use events::{
    ConnectionStateChange, Event, EventQueue, NetworkOperation, ScheduledEvent, StorageOperation,
};
pub use rng::{
    get_current_sim_seed, reset_sim_rng, set_sim_seed, sim_random, sim_random_f64,
    sim_random_range, sim_random_range_or_default,
};
pub use sleep::SleepFuture;
pub use state::{FileId, PendingOpType, PendingStorageOp, StorageFileState, StorageState};
pub use world::{SimWorld, WeakSimWorld};
