//! Network simulation and abstraction layer.
//!
//! This module provides trait-based networking that allows seamless swapping
//! between real Tokio networking and simulated networking for testing.

/// Network configuration and settings
pub mod config;

/// Core networking traits and abstractions
pub mod traits;

/// Real networking implementation using Tokio
pub mod tokio;

/// Simulated networking implementation for testing
pub mod sim;

/// Resilient peer connection management
pub mod peer;

/// Transport layer with Sans I/O architecture
pub mod transport;

// Re-export main traits
pub use traits::{NetworkProvider, TcpListenerTrait};

// Re-export implementations
pub use sim::SimNetworkProvider;
pub use tokio::TokioNetworkProvider;

// Re-export peer types
pub use peer::{Peer, PeerConfig, PeerError, PeerMetrics};

// Re-export configuration
pub use config::{
    CloggingConfiguration, LatencyConfiguration, LatencyRange, NetworkConfiguration,
    NetworkRandomizationRanges,
};

// Re-export transport types
pub use transport::{
    ClientTransport, EnvelopeSerializer, NetTransport, ReceivedEnvelope, RequestResponseEnvelope,
    RequestResponseSerializer, SerializationError, ServerTransport, Transmit, TransportDriver,
    TransportError, TransportProtocol,
};
