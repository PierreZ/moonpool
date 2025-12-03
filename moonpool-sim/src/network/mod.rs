//! Network simulation and configuration.
//!
//! This module provides simulated networking that integrates with the
//! deterministic simulation engine for testing.

/// Network configuration and settings
pub mod config;

/// Simulated networking implementation for testing
pub mod sim;

// Re-export configuration
pub use config::{ChaosConfiguration, ConnectFailureMode, NetworkConfiguration, sample_duration};

// Re-export simulation network provider
pub use sim::SimNetworkProvider;
