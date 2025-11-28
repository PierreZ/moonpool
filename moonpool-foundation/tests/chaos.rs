//! Chaos testing module.
//!
//! Contains tests for chaos injection and fault tolerance.

#[path = "chaos/assertions.rs"]
mod assertions;
#[path = "chaos/bit_flip.rs"]
mod bit_flip;
#[path = "chaos/buggified_delay.rs"]
mod buggified_delay;
#[path = "chaos/buggify.rs"]
mod buggify;
#[path = "chaos/clock_drift.rs"]
mod clock_drift;
#[path = "chaos/connect_failure.rs"]
mod connect_failure;
#[path = "chaos/random_close.rs"]
mod random_close;
