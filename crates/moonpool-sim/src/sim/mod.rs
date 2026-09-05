//! Core simulation engine for deterministic testing.
//!
//! This module provides the central SimWorld coordinator that manages time,
//! event processing, and network simulation state.
//!
//! ## Submodules
//!
//! - `world` - Core SimWorld and WeakSimWorld types
//! - `events` - Event types and queue for scheduling
//! - `wakers` - Waker management for async coordination
//! - `sleep` - Sleep future for simulation time
//! - `rng` - Thread-local random number generation

pub mod events;
pub mod rng;
pub mod sleep;
pub mod storage_ops;
pub mod wakers;
pub mod world;

// Re-export main types at module level
pub use events::{
    Event, ProcessKillKind, ScheduleError, ScheduleId, Scheduled, Scheduler, StorageOperation,
};
pub use rng::{
    DeterminismViolation, begin_determinism_check, begin_determinism_record, clear_rng_breakpoints,
    clear_swarm_op_mask, current_sim_seed, draw_swarm_op_mask, finish_determinism_check,
    install_select_offset, reset_rng_call_count, reset_sim_rng, rng_call_count,
    set_rng_breakpoints, set_sim_seed, sim_random, sim_random_bool, sim_random_f64,
    sim_random_range, sim_random_range_or_default, stop_determinism_canary, swarm_op_enabled,
    uninstall_select_offset,
};
pub use sleep::SleepFuture;
pub use world::{SimFaultRecord, SimWorld, WeakSimWorld};
