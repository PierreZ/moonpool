//! Resilient peer connection management.
//!
//! This module provides the Peer abstraction similar to FoundationDB's Peer class,
//! designed to work seamlessly with our NetworkProvider trait system for automatic
//! reconnection, fault tolerance, and message reliability.

/// Core peer implementation with automatic reconnection and queuing
pub mod core;

/// Configuration structures for peer behavior
pub mod config;

/// Metrics collection and connection state tracking
pub mod metrics;

/// Error types specific to peer operations
pub mod error;

// Re-export main types
pub use config::PeerConfig;
pub use core::{Peer, PeerReceiver};
pub use error::PeerError;
pub use metrics::PeerMetrics;
