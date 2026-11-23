//! Network simulation: TCP-like streams over logical time, and their faults.
//!
//! Code reaches the network through [`NetworkProvider`](crate::NetworkProvider).
//! In simulation the provider is a [`SimNetworkProvider`]: `bind`, `connect`
//! and `accept` complete after a simulated latency, and stream bytes travel
//! through the network engine in chunks, with a bounded send window per
//! direction and a bounded accept backlog per listener.
//!
//! # Configuration
//!
//! [`NetworkConfiguration`] holds latencies, windows and the fault knobs in
//! its [`ChaosConfiguration`]. [`Chaos::Network`](crate::Chaos::Network)
//! samples one per seed (`random_for_seed` or `swarm_for_seed`), and
//! [`SimulationBuilder::network_fault_mask`](crate::SimulationBuilder::network_fault_mask)
//! removes families ([`NetworkFault`], [`NetworkFaultMask`]) after sampling.
//!
//! | Fault family | What the code sees |
//! |--------------|--------------------|
//! | [`Clog`](NetworkFault::Clog) | Writes stall for a while (backpressure) |
//! | [`Partition`](NetworkFault::Partition) | Two nodes cannot talk, in one or both directions ([`PartitionStrategy`]) |
//! | [`BitFlip`](NetworkFault::BitFlip) | Bytes in flight are corrupted |
//! | [`RandomClose`](NetworkFault::RandomClose) | A connection closes during I/O |
//! | [`ConnectFailure`](NetworkFault::ConnectFailure) | `connect` fails or hangs ([`ConnectFailureMode`]) |
//! | [`ClockDrift`](NetworkFault::ClockDrift) | `timer()` reads up to `clock_drift_max` ahead of `now()` |
//! | [`BuggifiedDelay`](NetworkFault::BuggifiedDelay) | Timers fire late |
//! | [`PairLatency`](NetworkFault::PairLatency) | One IP pair stays slow for the whole run |
//! | [`BlackHole`](NetworkFault::BlackHole) | Sends on a connection vanish silently and forever |
//!
//! Partial reads and writes are TCP behavior, not a fault family: buggify
//! drives them, and the mask leaves them alone.
//!
//! # Distance
//!
//! [`LinkLatencyConfig`] gives each link a latency distribution
//! ([`LatencyDistribution`]) by how far apart the two processes are in a
//! [`cluster`](crate::SimulationBuilder::cluster) topology
//! ([`LinkClass`](crate::LinkClass)). It is realism, not chaos: it applies on
//! every seed.

/// Network configuration and settings
pub mod config;

/// Simulated networking implementation for testing
pub mod sim;

// Re-export configuration
pub use config::{
    ChaosConfiguration, ConnectFailureMode, LatencyDistribution, LinkLatencyConfig,
    NetworkConfiguration, NetworkFault, NetworkFaultMask, PartitionStrategy, sample_duration,
    sample_latency,
};

/// A guard the simulated network asks before it makes a random partition:
/// `guard(a, b, cut)` answers whether splitting the IPs `a` from `b` is
/// allowed (an empty `b` cuts `a` off from everyone), with `cut(x, y)`
/// saying whether `x` cannot reach `y` now. It must draw no randomness.
pub(crate) type PartitionGuard = std::sync::Arc<
    dyn Fn(
            &[std::net::IpAddr],
            &[std::net::IpAddr],
            &dyn Fn(std::net::IpAddr, std::net::IpAddr) -> bool,
        ) -> bool
        + Send
        + Sync,
>;

/// What a [`NetworkTap`] is shown: bytes the simulated network just put in
/// a receiver's buffer.
#[derive(Debug, Clone, Copy)]
pub struct DeliveredBytes<'a> {
    /// The connection the bytes crossed. Both directions of one connection
    /// share this number, and no other connection of the run has it.
    pub connection: u64,
    /// The sending endpoint: each direction of a connection has its own
    /// number, even when both ends share an IP.
    pub sender: u64,
    /// The sender's IP, when the sim knows it.
    pub from: Option<std::net::IpAddr>,
    /// The receiver's IP, when the sim knows it.
    pub to: Option<std::net::IpAddr>,
    /// The bytes, as the receiver gets them (after any bit flip).
    pub bytes: &'a [u8],
}

/// An observer of every delivery of stream bytes, installed with
/// [`SimContext::tap_network`](crate::SimContext::tap_network).
///
/// A tap lets an oracle learn from the network itself what crossed it,
/// instead of from the code under test. It is called while the simulation
/// holds its own lock: it must not call back into the simulation, wake a
/// task or draw randomness. It changes nothing in the run.
pub type NetworkTap = std::sync::Arc<dyn Fn(&DeliveredBytes<'_>) + Send + Sync>;

// Re-export simulation network provider
pub use sim::{DeathNotice, SimNetworkProvider};
