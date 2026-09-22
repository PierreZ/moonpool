//! Chaos testing module.
//!
//! Contains tests for chaos injection and fault tolerance.

#[path = "common/async_drive.rs"]
mod async_drive;
#[path = "common/runtime.rs"]
mod runtime;

use async_drive::drive;
use runtime::local_runtime;

#[path = "chaos/bit_flip.rs"]
mod bit_flip;
#[path = "chaos/black_hole.rs"]
mod black_hole;
#[path = "chaos/buggified_delay.rs"]
mod buggified_delay;
#[path = "chaos/buggify.rs"]
mod buggify;
#[path = "chaos/clock_drift.rs"]
mod clock_drift;
#[path = "chaos/connect_failure.rs"]
mod connect_failure;
#[path = "chaos/partial_read.rs"]
mod partial_read;
#[path = "chaos/random_close.rs"]
mod random_close;
#[path = "chaos/swarm.rs"]
mod swarm;
