//! Network tests module.
//!
//! Contains tests for network simulation and configuration.

#[path = "network/half_close.rs"]
mod half_close;
#[path = "network/latency.rs"]
mod latency;
#[path = "network/partition.rs"]
mod partition;
#[path = "network/traits.rs"]
mod traits;
