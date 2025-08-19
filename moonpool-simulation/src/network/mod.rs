//! Network simulation and abstraction layer.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

/// Core networking traits and abstractions
pub mod traits;

/// Real networking implementation using Tokio
pub mod tokio;

// Re-export main traits
pub use traits::{NetworkProvider, TcpListenerTrait};

// Re-export Tokio implementation
pub use tokio::TokioNetworkProvider;
