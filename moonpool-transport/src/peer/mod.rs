//! Resilient peer connection management.
//!
//! Provides the [`Peer`] abstraction following FoundationDB's FlowTransport
//! patterns (`FlowTransport.actor.cpp:1016-1125`). Handles automatic
//! reconnection with exponential backoff and message queuing during outages.
//!
//! Backoff follows FDB (`FlowTransport.actor.cpp:892-897`): initial delay,
//! max delay, exponential growth — all configurable via [`PeerConfig`].

/// Core peer implementation with automatic reconnection and queuing
pub mod core;

/// Configuration structures for peer behavior
pub mod config;

/// Metrics collection and connection state tracking
pub mod metrics;

/// Error types specific to peer operations
pub mod error;

// Re-export main types
pub use config::{MonitorConfig, PeerConfig};
pub use core::{Peer, PeerReceiver};
pub use error::PeerError;
pub use metrics::PeerMetrics;
