//! Network tests module.
//!
//! Contains tests for network simulation and configuration.

#[path = "network/hyper_http.rs"]
mod hyper_http;
#[path = "network/latency.rs"]
mod latency;
#[path = "network/partition.rs"]
mod partition;
#[path = "network/traits.rs"]
mod traits;
