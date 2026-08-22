//! Network tests module.
//!
//! Contains tests for network simulation and configuration.

#[path = "network/latency.rs"]
mod latency;
#[path = "network/partition.rs"]
mod partition;
#[path = "network/partition_strategy.rs"]
mod partition_strategy;
// Exercises TokioNetworkProvider — only available with the tokio-providers feature.
#[cfg(feature = "tokio-providers")]
#[path = "network/traits.rs"]
mod traits;
#[path = "network/vectored.rs"]
mod vectored;
