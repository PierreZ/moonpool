//! Convenience type aliases for common transport configurations.
//!
//! This module provides pre-configured type aliases that eliminate the need to specify
//! generic parameters for common use cases, greatly improving the developer experience.

use super::{ClientTransport, RequestResponseSerializer, ServerTransport};

// Simulation provider imports
use crate::network::SimNetworkProvider;
use crate::time::SimTimeProvider;

// Common provider combinations for real-world usage
use crate::network::TokioNetworkProvider;
use crate::task::tokio_provider::TokioTaskProvider;
use crate::time::TokioTimeProvider;

/// Simple client transport using RequestResponse envelopes.
///
/// This is the most basic client transport configuration using the built-in
/// RequestResponseEnvelope with the standard serializer.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::SimpleClient;
///
/// let client = SimpleClient::new(
///     RequestResponseSerializer::new(),
///     network_provider,
///     time_provider,
///     task_provider,
///     PeerConfig::default(),
/// );
/// ```
pub type SimpleClient<N, T, TP> = ClientTransport<N, T, TP, RequestResponseSerializer>;

/// Simple server transport using RequestResponse envelopes.
///
/// This is the most basic server transport configuration using the built-in
/// RequestResponseEnvelope with the standard serializer.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::SimpleServer;
///
/// let server = SimpleServer::new(
///     RequestResponseSerializer::new(),
///     network_provider,
///     time_provider,
///     task_provider,
///     peer_config,
/// );
/// ```
pub type SimpleServer<N, T, TP> = ServerTransport<N, T, TP, RequestResponseSerializer>;

// Real-world (Tokio) transport aliases
/// Tokio-based client transport for production use.
///
/// Pre-configured with all the Tokio providers for real networking, time, and task execution.
/// Uses the standard RequestResponse envelope format.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::TokioClient;
///
/// let client = TokioClient::new(
///     RequestResponseSerializer::new(),
///     TokioNetworkProvider,
///     TokioTimeProvider,
///     TokioTaskProvider,
///     PeerConfig::default(),
/// );
/// ```
pub type TokioClient = SimpleClient<TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider>;

/// Tokio-based server transport for production use.
///
/// Pre-configured with all the Tokio providers for real networking, time, and task execution.
/// Uses the standard RequestResponse envelope format.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::TokioServer;
///
/// let server = TokioServer::new(
///     RequestResponseSerializer::new(),
///     TokioNetworkProvider,
///     TokioTimeProvider,
///     TokioTaskProvider,
///     peer_config,
/// );
/// ```
pub type TokioServer = SimpleServer<TokioNetworkProvider, TokioTimeProvider, TokioTaskProvider>;

// Simulation transport aliases
/// Simulation-based client transport for testing.
///
/// Pre-configured with all the simulation providers for deterministic testing.
/// Uses the standard RequestResponse envelope format.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::SimClient;
///
/// let client = SimClient::new(
///     RequestResponseSerializer::new(),
///     sim_network_provider,
///     sim_time_provider,
///     task_provider,
///     PeerConfig::default(),
/// );
/// ```
pub type SimClient<TP> = SimpleClient<SimNetworkProvider, SimTimeProvider, TP>;

/// Simulation-based server transport for testing.
///
/// Pre-configured with all the simulation providers for deterministic testing.
/// Uses the standard RequestResponse envelope format.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::SimServer;
///
/// let server = SimServer::new(
///     RequestResponseSerializer::new(),
///     sim_network_provider,
///     sim_time_provider,
///     task_provider,
///     peer_config,
/// );
/// ```
pub type SimServer<TP> = SimpleServer<SimNetworkProvider, SimTimeProvider, TP>;

/// Complete simulation client using Tokio task provider.
///
/// This is the most convenient type alias for simulation testing, combining
/// simulation networking and time with Tokio task execution.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::SimTokioClient;
///
/// let client = SimTokioClient::new(
///     RequestResponseSerializer::new(),
///     sim_network_provider,
///     sim_time_provider,
///     TokioTaskProvider,
///     PeerConfig::default(),
/// );
/// ```
pub type SimTokioClient = SimClient<TokioTaskProvider>;

/// Complete simulation server using Tokio task provider.
///
/// This is the most convenient type alias for simulation testing, combining
/// simulation networking and time with Tokio task execution.
///
/// # Example
///
/// ```rust
/// use moonpool_foundation::network::transport::SimTokioServer;
///
/// let server = SimTokioServer::new(
///     RequestResponseSerializer::new(),
///     sim_network_provider,
///     sim_time_provider,
///     TokioTaskProvider,
///     peer_config,
/// );
/// ```
pub type SimTokioServer = SimServer<TokioTaskProvider>;
