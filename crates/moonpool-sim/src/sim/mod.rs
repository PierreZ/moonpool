//! The simulation engine: the world, its scheduler, its clock and its one
//! random stream.
//!
//! [`SimWorld`] is the coordinator. Its global [`Scheduler<Event>`](Scheduler)
//! owns logical time and dispatches targeted events to two independent
//! engines: the network engine ([`crate::network`]) and the storage engine
//! ([`crate::storage`]). Both return ordered effects and wake batches, and
//! the world runs the wakers only after it releases its lock. Time moves
//! only when the world steps to the next event, so idle time costs nothing.
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`world`] | [`SimWorld`], [`WeakSimWorld`], fault records, recovery mode |
//! | [`events`] | [`Event`], [`Scheduler`], [`ScheduleId`] |
//! | [`rng`] | The one counted random stream, swarm masks, breakpoints, the determinism canary |
//! | [`cpu`] | The CPU gate: [`CpuModel`] and per-machine [`Slowness`] |
//! | [`sleep`] | [`SleepFuture`] over logical time |
//! | [`storage_ops`] | `SimWorld` methods that reach the storage engine |
//! | [`wakers`] | Waker registries and wake batches |
//!
//! Most users never touch this module: the
//! [`SimulationBuilder`](crate::SimulationBuilder) builds one world per seed.
//! Its public functions serve tests that drive providers by hand, harnesses
//! that inspect the world, and the determinism tools
//! ([`rng_call_count`], [`set_rng_breakpoints`], [`swarm_op_enabled`]).

pub mod cpu;
pub mod events;
pub mod rng;
pub mod sleep;
pub mod storage_ops;
pub mod wakers;
pub mod world;

// Re-export main types at module level
pub use cpu::{CpuModel, Slowness};
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
pub use world::{SYSTEM_SAMPLES_KEPT, SimFaultRecord, SimWorld, WeakSimWorld};
