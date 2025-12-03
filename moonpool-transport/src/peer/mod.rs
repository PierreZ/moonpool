//! Resilient peer connection management.
//!
//! This module provides the [`Peer`] abstraction following FoundationDB's
//! FlowTransport patterns (`FlowTransport.actor.cpp:1016-1125`).
//!
//! # Overview
//!
//! A Peer represents a logical connection to a remote endpoint. It handles:
//! - **Automatic reconnection** with exponential backoff
//! - **Message queuing** during disconnection periods
//! - **Graceful degradation** under network failures
//!
//! # Connection Lifecycle
//!
//! ```text
//! ┌──────────┐    connect     ┌───────────┐
//! │Disconnect├───────────────►│ Connected │
//! │   ed     │◄───────────────┤           │
//! └────┬─────┘  conn. error   └─────┬─────┘
//!      │                            │
//!      │ backoff                    │ send/recv
//!      ▼                            ▼
//! ┌──────────┐              ┌───────────┐
//! │Reconnect │              │   Active  │
//! │  ing     │              │   I/O     │
//! └──────────┘              └───────────┘
//! ```
//!
//! # Backoff Strategy
//!
//! Follows FDB patterns (`FlowTransport.actor.cpp:892-897`):
//! - Initial delay: configurable (default 100ms)
//! - Maximum delay: configurable (default 5s)
//! - Exponential growth with jitter
//!
//! # Configuration
//!
//! ```ignore
//! use moonpool_transport::PeerConfig;
//!
//! let config = PeerConfig::default()
//!     .with_initial_backoff(Duration::from_millis(100))
//!     .with_max_backoff(Duration::from_secs(5));
//! ```
//!
//! # Chaos Testing
//!
//! Under simulation, peers experience:
//! - Random connection closes (0.001%)
//! - Connection failures (50% probabilistic)
//! - Partial writes
//! - Half-open connection simulation

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
