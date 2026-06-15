//! Network simulation and configuration.
//!
//! This module provides simulated networking that integrates with the
//! deterministic simulation engine for testing.

/// Network configuration and settings
pub mod config;

/// Simulated networking implementation for testing
pub mod sim;

// Re-export configuration
pub use config::{
    ChaosConfiguration, ConnectFailureMode, LatencyDistribution, NetworkConfiguration,
    PartitionStrategy, sample_duration, sample_latency,
};

// Re-export simulation network provider
pub use sim::SimNetworkProvider;
