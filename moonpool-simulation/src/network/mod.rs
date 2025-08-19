//! Network simulation and abstraction layer.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

/// Core networking traits and abstractions
pub mod traits;

/// Real networking implementation using Tokio
pub mod tokio;

/// Simulated networking implementation for testing
pub mod sim;

// Re-export main traits
pub use traits::{NetworkProvider, TcpListenerTrait};

// Re-export implementations
pub use sim::SimNetworkProvider;
pub use tokio::TokioNetworkProvider;
