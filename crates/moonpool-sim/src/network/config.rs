//! # Network Chaos Configuration
//!
//! This module provides configuration for network chaos testing, following
//! FoundationDB's battle-tested simulation approach and TigerBeetle's deterministic
//! testing patterns.
//!
//! ## Connection Failure Modes
//!
//! | Failure | Config Field | Default | Real-World Scenario |
//! |---------|--------------|---------|---------------------|
//! | Random close | `random_close_probability` | 0.001% | Reconnection logic, message redelivery, connection pooling |
//! | Close error surfacing | `random_close_explicit_ratio` | 30% explicit | Immediate-error vs timeout-based detection |
//! | Connect failure | `connect_failure_mode` | Probabilistic | Connection establishment retries, timeout handling |
//!
//! ## Network Latency & Congestion
//!
//! | Delay Type | Config Field | Default | Real-World Scenario |
//! |------------|--------------|---------|---------------------|
//! | Operation latency | `bind/accept/connect/write_latency` | Various ranges | Timeout settings, async operation ordering |
//! | Write clogging | `clog_probability` + `clog_duration` | 0%, 100-300ms | Backpressure handling, flow control |
//! | Read clogging | Same as write | Same | Symmetric flow control |
//! | Clock drift | `clock_drift_enabled` + `clock_drift_max` | true, 100ms | Lease expiration, distributed consensus, TTL handling |
//! | Buggified delay | `buggified_delay_enabled` + `buggified_delay_max` | true, 100ms | Race conditions, timing-dependent bugs |
//!
//! ## Network Partitions
//!
//! | Partition Type | Config/Method | Default | Real-World Scenario |
//! |----------------|---------------|---------|---------------------|
//! | Random partition | `partition_probability` + `partition_duration` | 0%, 200ms-2s | Split-brain, quorum loss, leader election |
//! | Directed pair | `partition_pair()` | Manual | One-way loss between two nodes |
//! | Pair restore | `restore_partition()` | Manual | Heals both directed pair entries |
//! | Send-only block | `partition_send_from()` | Manual | Asymmetric network failures |
//! | Recv-only block | `partition_recv_to()` | Manual | Asymmetric network failures |
//! | Partition strategy | `partition_strategy` | Random | Different failure patterns (uniform, isolate) |
//! | Zone / datacenter cut | `partition_strategy = IsolateZone` / `IsolateDatacenter` | Random | Rack or region loss (needs a locality topology) |
//! | One-way cut | `partition_strategy = AsymmetricSend` / `AsymmetricRecv` | Random | Half-reachable node, failure detector confusion |
//!
//! ## Data Integrity Faults
//!
//! | Fault | Config Field | Default | Real-World Scenario |
//! |-------|--------------|---------|---------------------|
//! | Bit flip | `bit_flip_probability` + `bit_flip_min/max_bits` | 0.01%, 1-32 bits | CRC/checksum validation, data corruption detection |
//!
//! ## Partial Write/Read Simulation
//!
//! | Feature | Config Field | Default | Real-World Scenario |
//! |---------|--------------|---------|---------------------|
//! | Short writes | `partial_write_max_bytes` | 1000 bytes | TCP fragmentation handling, message framing |
//! | Short reads | `partial_read_max_bytes` | 1000 bytes | TCP short reads, message reassembly / framing |
//!
//! ## Configuration Examples
//!
//! ### Fast Local Testing (No Chaos)
//! ```rust
//! use moonpool_sim::network::{NetworkConfiguration, ChaosConfiguration};
//!
//! let config = NetworkConfiguration::fast_local();
//! // All chaos disabled, minimal latencies
//! ```
//!
//! ### Full Chaos Testing
//! ```rust
//! use moonpool_sim::network::{NetworkConfiguration, ChaosConfiguration};
//!
//! let config = NetworkConfiguration::random_for_seed();
//! // Randomized chaos parameters for comprehensive testing
//! ```
//!
//! ### Custom Configuration
//! ```rust
//! use moonpool_sim::network::{NetworkConfiguration, PartitionStrategy};
//!
//! let mut config = NetworkConfiguration::default();
//! config.chaos.partition_strategy = PartitionStrategy::IsolateSingle;
//! config.chaos.partition_probability = 0.05; // 5%
//! ```
//!
//! ## FDB/TigerBeetle References
//!
//! - Random close: FDB sim2.actor.cpp:580-605
//! - Partitions: FDB SimClogging, TigerBeetle partition modes
//! - Bit flips: FDB FlowTransport.actor.cpp:1297
//! - Clock drift: FDB sim2.actor.cpp:1058-1064
//! - Connect failures: FDB sim2.actor.cpp:1243-1250

use crate::locality::LinkClass;
use crate::sim::rng::{
    config_random_bool, sim_random_f64, sim_random_range, sim_random_range_or_default,
};
use std::ops::Range;
use std::time::Duration;

/// Network partition strategy for chaos testing.
///
/// Controls how nodes are selected for partitioning during chaos testing.
/// `TigerBeetle` ref: packet_simulator.zig:12-488
///
/// # Real-World Scenario
///
/// Different partition strategies test different failure modes:
/// - Random: General chaos, unpredictable failures
/// - `UniformSize`: Tests various quorum sizes and split scenarios
/// - `IsolateSingle`: Tests single-node isolation (common in production)
/// - `IsolateZone` / `IsolateDatacenter`: Tests correlated, topology-shaped cuts
///   (rack or region loss)
/// - `AsymmetricSend` / `AsymmetricRecv`: Tests one-way reachability, where a node
///   still hears the cluster but cannot answer (or the reverse)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionStrategy {
    /// Random IP pairs selected for partitioning.
    /// Current behavior - randomly selects which connections to partition.
    #[default]
    Random,

    /// Uniform size partitions - randomly choose partition size from 1 to n-1 nodes.
    /// `TigerBeetle` pattern: creates partitions of varying sizes to test different
    /// quorum scenarios.
    UniformSize,

    /// Isolate single node - always partition exactly one node from the rest.
    /// Tests the common production scenario where a single node becomes unreachable.
    IsolateSingle,

    /// Isolate a whole zone - cut every process of one random zone from the rest.
    ///
    /// Models a rack or availability-zone loss, where collocated processes fail
    /// together. Requires a locality topology
    /// ([`SimWorld::set_localities`](crate::SimWorld::set_localities), installed by
    /// [`.cluster()`](crate::SimulationBuilder::cluster)); without one it degrades
    /// to [`Random`](Self::Random) selection.
    IsolateZone,

    /// Isolate a whole datacenter - cut every process of one random datacenter
    /// from the rest.
    ///
    /// Models a region-level network cut, the classic cross-datacenter replication
    /// test. Degrades to [`Random`](Self::Random) without a locality topology.
    IsolateDatacenter,

    /// Block all *outgoing* traffic from one random node (one-way cut).
    ///
    /// The node still receives, so it keeps seeing cluster traffic while its own
    /// replies vanish (the failure mode that breaks naive failure detectors).
    /// FDB models the same asymmetry with its send-side clogging.
    AsymmetricSend,

    /// Block all *incoming* traffic to one random node (one-way cut).
    ///
    /// The mirror of [`AsymmetricSend`](Self::AsymmetricSend): the node keeps
    /// sending, but hears nothing back.
    AsymmetricRecv,
}

/// Connection establishment failure mode for fault injection.
///
/// Controls how connection attempts fail during chaos testing.
/// FDB ref: sim2.actor.cpp:1243-1250 (`SIM_CONNECT_ERROR_MODE`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectFailureMode {
    /// Disabled - no connection failures injected
    #[default]
    Disabled,
    /// Always fail with `ConnectionRefused` when buggified
    AlwaysFail,
    /// Probabilistic: 50% fail with `ConnectionRefused`, 50% hang forever
    Probabilistic,
}

/// A network fault family that can be retained or suppressed by a
/// [`NetworkFaultMask`].
///
/// The mask only suppresses faults selected by a network chaos profile; it
/// never enables a family whose sampled probability or mode is already off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFault {
    /// Write-delivery clogging.
    Clog,
    /// Directed network partitions.
    Partition,
    /// In-flight data bit-flip corruption.
    BitFlip,
    /// Spontaneous connection closes during I/O.
    RandomClose,
    /// Connection-establishment failures and hangs.
    ConnectFailure,
    /// Per-node simulated clock drift.
    ClockDrift,
    /// Buggify-driven extra timer delay.
    BuggifiedDelay,
    /// Permanent per-IP-pair latency degradation.
    PairLatency,
}

impl NetworkFault {
    const fn bit(self) -> u8 {
        match self {
            Self::Clog => 1 << 0,
            Self::Partition => 1 << 1,
            Self::BitFlip => 1 << 2,
            Self::RandomClose => 1 << 3,
            Self::ConnectFailure => 1 << 4,
            Self::ClockDrift => 1 << 5,
            Self::BuggifiedDelay => 1 << 6,
            Self::PairLatency => 1 << 7,
        }
    }
}

/// A deterministic allow-mask for per-seed network fault profiles.
///
/// [`NetworkFaultMask::all`] is the default. Removing a family makes that
/// family inert after the builder has sampled its Random or Swarm profile,
/// without consuming either simulation or configuration randomness. This
/// makes the mask safe for frontier exploration and exact recipe replay.
///
/// Partial reads and writes are TCP behavior exercised by buggify rather than
/// independently sampled fault families, so this mask deliberately leaves
/// them unchanged.
///
/// # Example
///
/// ```
/// use moonpool_sim::{NetworkFault, NetworkFaultMask};
///
/// let mask = NetworkFaultMask::all().without(NetworkFault::BitFlip);
/// assert!(!mask.contains(NetworkFault::BitFlip));
/// assert!(mask.contains(NetworkFault::Partition));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkFaultMask(u8);

impl Default for NetworkFaultMask {
    fn default() -> Self {
        Self::all()
    }
}

impl NetworkFaultMask {
    const ALL: u8 = u8::MAX;

    /// Retain every selected network fault family.
    #[must_use]
    pub const fn all() -> Self {
        Self(Self::ALL)
    }

    /// Suppress every network fault family.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Return a mask that also retains `fault`.
    #[must_use]
    pub const fn with(self, fault: NetworkFault) -> Self {
        Self(self.0 | fault.bit())
    }

    /// Return a mask that suppresses `fault`.
    #[must_use]
    pub const fn without(self, fault: NetworkFault) -> Self {
        Self(self.0 & !fault.bit())
    }

    /// Return whether `fault` is retained by this mask.
    #[must_use]
    pub const fn contains(self, fault: NetworkFault) -> bool {
        self.0 & fault.bit() != 0
    }

    pub(crate) fn apply_to(self, chaos: &mut ChaosConfiguration) {
        if !self.contains(NetworkFault::Clog) {
            chaos.clog_probability = 0.0;
        }
        if !self.contains(NetworkFault::Partition) {
            chaos.partition_probability = 0.0;
        }
        if !self.contains(NetworkFault::BitFlip) {
            chaos.bit_flip_probability = 0.0;
        }
        if !self.contains(NetworkFault::RandomClose) {
            chaos.random_close_probability = 0.0;
        }
        if !self.contains(NetworkFault::ConnectFailure) {
            chaos.connect_failure_mode = ConnectFailureMode::Disabled;
            chaos.connect_failure_probability = 0.0;
        }
        if !self.contains(NetworkFault::ClockDrift) {
            chaos.clock_drift_enabled = false;
        }
        if !self.contains(NetworkFault::BuggifiedDelay) {
            chaos.buggified_delay_enabled = false;
        }
        if !self.contains(NetworkFault::PairLatency) {
            chaos.max_pair_latency = Duration::ZERO..Duration::ZERO;
        }
    }
}

impl ConnectFailureMode {
    /// Create a random failure mode for chaos testing
    #[must_use]
    pub fn random_for_seed() -> Self {
        match sim_random_range(0..3) {
            0 => Self::Disabled,
            1 => Self::AlwaysFail,
            _ => Self::Probabilistic,
        }
    }
}

/// Configuration for chaos injection in simulations.
///
/// This struct contains all settings related to fault injection and chaos testing,
/// following `FoundationDB`'s BUGGIFY patterns for deterministic testing.
#[derive(Debug, Clone, PartialEq)]
pub struct ChaosConfiguration {
    /// Clogging probability for individual writes (0.0 - 1.0)
    pub clog_probability: f64,
    /// Duration range for clog delays
    pub clog_duration: Range<Duration>,

    /// Network partition probability (0.0 - 1.0)
    pub partition_probability: f64,
    /// Duration range for network partitions
    pub partition_duration: Range<Duration>,

    /// Bit flip probability for packet corruption (0.0 - 1.0)
    pub bit_flip_probability: f64,
    /// Minimum number of bits to flip (power-law distribution lower bound)
    pub bit_flip_min_bits: u32,
    /// Maximum number of bits to flip (power-law distribution upper bound)
    pub bit_flip_max_bits: u32,
    /// Cooldown duration after bit flip to prevent excessive corruption
    pub bit_flip_cooldown: Duration,

    /// Maximum bytes for partial write simulation (BUGGIFY truncates writes to 0-max_bytes)
    /// Following FDB's approach of truncating writes to test TCP backpressure handling
    pub partial_write_max_bytes: usize,

    /// Maximum bytes for partial read simulation (BUGGIFY truncates reads to 1-max_bytes)
    /// Mirrors FDB's `Sim2Conn` partial delivery on the receiver to test short reads
    /// and message reassembly. Always delivers at least one byte so a partial read is
    /// never mistaken for EOF.
    pub partial_read_max_bytes: usize,

    /// Random connection close probability per I/O operation (0.0 - 1.0)
    /// FDB default: 0.00001 (0.001%) - see sim2.actor.cpp:584
    pub random_close_probability: f64,

    /// Cooldown duration after a random close event (prevents cascading failures)
    /// FDB uses connectionFailuresDisableDuration - see sim2.actor.cpp:583
    pub random_close_cooldown: Duration,

    /// Ratio of explicit exceptions vs silent failures (0.0 - 1.0)
    /// FDB default: 0.3 (30% explicit) - see sim2.actor.cpp:602
    pub random_close_explicit_ratio: f64,

    /// Enable clock drift simulation
    /// When enabled, `timer()` can return a time up to `clock_drift_max` ahead of `now()`
    /// FDB ref: sim2.actor.cpp:1058-1064
    pub clock_drift_enabled: bool,

    /// Maximum clock drift (default 100ms per FDB)
    /// `timer()` can be up to this much ahead of `now()`
    pub clock_drift_max: Duration,

    /// Enable buggified delays on sleep/timer operations
    /// When enabled, 25% of sleep operations get extra delay
    /// FDB ref: sim2.actor.cpp:1100-1105
    pub buggified_delay_enabled: bool,

    /// Maximum additional delay for buggified sleep (default 100ms)
    /// Uses power-law distribution: `max_delay` * `pow(random01()`, 1000.0)
    /// FDB ref: sim2.actor.cpp:1104
    pub buggified_delay_max: Duration,

    /// Probability of adding buggified delay (default 25% per FDB)
    pub buggified_delay_probability: f64,

    /// Connection establishment failure mode (per FDB)
    /// FDB ref: sim2.actor.cpp:1243-1250 (`SIM_CONNECT_ERROR_MODE`)
    pub connect_failure_mode: ConnectFailureMode,

    /// Probability of connect failure when Probabilistic mode is enabled (default 50%)
    pub connect_failure_probability: f64,

    /// Permanent per-IP-pair latency range (FDB `SimClogging::MAX_CLOGGING_LATENCY`).
    /// Each ordered IP pair samples a fixed latency from this range at first contact
    /// (via [`sample_duration`]), then adds it to every delivery on that pair for the
    /// whole run — modelling a stably-slow link. An all-zero range (`end` is zero)
    /// disables it, leaving behavior unchanged. FDB's `MAX * random01()` is the
    /// `ZERO..MAX` case. FDB ref: `sim2.actor.cpp` ~294-299, 352-354.
    pub max_pair_latency: Range<Duration>,

    /// Network partition strategy.
    /// Controls how nodes are selected for partitioning.
    /// `TigerBeetle` ref: `packet_simulator.zig` partition modes
    ///
    /// # Real-World Scenario
    /// Different strategies test different failure scenarios:
    /// - Random: unpredictable chaos
    /// - `UniformSize`: various quorum sizes
    /// - `IsolateSingle`: single node isolation (common in production)
    pub partition_strategy: PartitionStrategy,
}

impl Default for ChaosConfiguration {
    fn default() -> Self {
        Self {
            clog_probability: 0.0,
            clog_duration: Duration::from_millis(100)..Duration::from_millis(300),
            partition_probability: 0.0,
            partition_duration: Duration::from_millis(200)..Duration::from_secs(2),
            bit_flip_probability: 0.0001, // 0.01% - matches FDB's BUGGIFY_WITH_PROB(0.0001)
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::ZERO, // No cooldown by default for maximum chaos
            partial_write_max_bytes: 1000,     // Matches FDB's randomInt(0, 1000)
            partial_read_max_bytes: 1000,      // Symmetric with partial writes
            random_close_probability: 0.00001, // 0.001% - matches FDB's sim2.actor.cpp:584
            random_close_cooldown: Duration::from_secs(5), // Reasonable default
            random_close_explicit_ratio: 0.3,  // 30% explicit - matches FDB's sim2.actor.cpp:602
            clock_drift_enabled: true,         // Enable by default for chaos testing
            clock_drift_max: Duration::from_millis(100), // FDB default: 0.1 seconds
            buggified_delay_enabled: true,     // Enable by default for chaos testing
            buggified_delay_max: Duration::from_millis(100), // FDB: MAX_BUGGIFIED_DELAY
            buggified_delay_probability: 0.25, // FDB: random01() < 0.25
            connect_failure_mode: ConnectFailureMode::Probabilistic, // FDB: SIM_CONNECT_ERROR_MODE = 2
            connect_failure_probability: 0.5,                        // FDB: random01() > 0.5
            max_pair_latency: Duration::ZERO..Duration::ZERO, // FDB: MAX_CLOGGING_LATENCY default 0
            partition_strategy: PartitionStrategy::default(),
        }
    }
}

impl ChaosConfiguration {
    /// Create a configuration with all chaos disabled (for fast local testing)
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            clog_probability: 0.0,
            clog_duration: Duration::ZERO..Duration::ZERO,
            partition_probability: 0.0,
            partition_duration: Duration::ZERO..Duration::ZERO,
            bit_flip_probability: 0.0,
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::ZERO,
            partial_write_max_bytes: 1000,
            partial_read_max_bytes: 1000,
            random_close_probability: 0.0,
            random_close_cooldown: Duration::ZERO,
            random_close_explicit_ratio: 0.3,
            clock_drift_enabled: false,
            clock_drift_max: Duration::from_millis(100),
            buggified_delay_enabled: false,
            buggified_delay_max: Duration::from_millis(100),
            buggified_delay_probability: 0.25,
            connect_failure_mode: ConnectFailureMode::Disabled,
            connect_failure_probability: 0.5,
            max_pair_latency: Duration::ZERO..Duration::ZERO,
            partition_strategy: PartitionStrategy::Random, // Default strategy
        }
    }

    /// Create a randomized chaos configuration for seed-based testing
    #[must_use]
    pub fn random_for_seed() -> Self {
        Self {
            clog_probability: f64::from(sim_random_range(0..20)) / 100.0, // 0-20% for clogging
            clog_duration: Duration::from_micros(sim_random_range(50_000..300_000))
                ..Duration::from_micros(sim_random_range(100_000..500_000)),
            partition_probability: f64::from(sim_random_range(0..15)) / 100.0, // 0-15% (lower than faults)
            partition_duration: Duration::from_millis(sim_random_range(100..1000))
                ..Duration::from_millis(sim_random_range(500..3000)),
            // Bit flip probability range: 0.001% to 0.02% (very low, like FDB)
            bit_flip_probability: f64::from(sim_random_range(1..20)) / 100_000.0,
            bit_flip_min_bits: 1,
            bit_flip_max_bits: 32,
            bit_flip_cooldown: Duration::from_millis(sim_random_range(0..100)),
            partial_write_max_bytes: sim_random_range(100..2000), // Vary max bytes for different scenarios
            // Random close probability: 0.0001% to 0.01% (very low, like FDB)
            random_close_probability: f64::from(sim_random_range(1..100)) / 1_000_000.0,
            random_close_cooldown: Duration::from_millis(sim_random_range(1000..10_000)),
            random_close_explicit_ratio: f64::from(sim_random_range(20..40)) / 100.0, // 20-40%
            clock_drift_enabled: true,
            clock_drift_max: Duration::from_millis(sim_random_range(50..150)), // 50-150ms
            buggified_delay_enabled: true,
            buggified_delay_max: Duration::from_millis(sim_random_range(50..150)), // 50-150ms
            buggified_delay_probability: f64::from(sim_random_range(20..30)) / 100.0, // 20-30%
            connect_failure_mode: ConnectFailureMode::random_for_seed(),
            connect_failure_probability: f64::from(sim_random_range(40..60)) / 100.0, // 40-60%
            // Randomly choose partition strategy. The locality-shaped arms
            // degrade to `Random` selection when the run has no topology, so
            // they are safe to draw for every seed.
            partition_strategy: match sim_random_range(0..7) {
                0 => PartitionStrategy::Random,
                1 => PartitionStrategy::UniformSize,
                2 => PartitionStrategy::IsolateSingle,
                3 => PartitionStrategy::IsolateZone,
                4 => PartitionStrategy::IsolateDatacenter,
                5 => PartitionStrategy::AsymmetricSend,
                _ => PartitionStrategy::AsymmetricRecv,
            },
            // Permanent per-pair latency, randomized upper bound up to 100ms (FDB
            // buggifies MAX_CLOGGING_LATENCY to 0.1s). Kept last so existing RNG
            // draws above are unaffected.
            max_pair_latency: Duration::ZERO..Duration::from_millis(sim_random_range(0..100)),
            // Vary max bytes for different scenarios. Appended last (after
            // max_pair_latency) so the RNG draws above keep their per-seed values.
            partial_read_max_bytes: sim_random_range(100..2000),
        }
    }

    /// Create a swarm-testing chaos configuration for seed-based testing.
    ///
    /// Starts from [`random_for_seed`](Self::random_for_seed), then disables each
    /// fault family with ~50% probability (drawn from the independent `CONFIG_RNG`
    /// stream). This implements *swarm testing* (Groce et al., ISSTA 2012): each
    /// seed exercises a random *subset* of fault families — including the all-off
    /// subset — instead of every family being slightly on at once (which lets
    /// families crowd each other out, the passive-suppression anti-pattern).
    #[must_use]
    pub fn swarm_for_seed() -> Self {
        let mut chaos = Self::random_for_seed();
        chaos.apply_swarm_mask();
        chaos
    }

    /// Disable each fault family with ~50% probability using the `CONFIG_RNG`
    /// stream (see [`swarm_for_seed`](Self::swarm_for_seed)).
    ///
    /// Draws exactly eight `config_random_bool` values (one per family) so the
    /// `CONFIG_RNG` call sequence is fixed and reproducible per seed. Durations,
    /// cooldowns, and strategy stay as sampled — they are inert once their family
    /// is off.
    fn apply_swarm_mask(&mut self) {
        if !config_random_bool(0.5) {
            self.clog_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.partition_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.bit_flip_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.random_close_probability = 0.0;
        }
        if !config_random_bool(0.5) {
            self.connect_failure_mode = ConnectFailureMode::Disabled;
            self.connect_failure_probability = 0.0;
        }
        self.clock_drift_enabled = config_random_bool(0.5);
        self.buggified_delay_enabled = config_random_bool(0.5);
        // Appended last: keeps the seven draws above stable across seeds.
        if !config_random_bool(0.5) {
            self.max_pair_latency = Duration::ZERO..Duration::ZERO;
        }
    }

    /// Spike selected fault *magnitudes* under buggify (FDB's
    /// `if (randomize && BUGGIFY) KNOB = random(lo, hi)`).
    ///
    /// Composes on top of [`random_for_seed`](Self::random_for_seed) /
    /// [`swarm_for_seed`](Self::swarm_for_seed): each knob keeps its sampled value
    /// unless its own [`buggify_knob!`](crate::buggify_knob) call site fires for the
    /// seed, in which case it jumps to an aggressive value within bounds. A
    /// representative subset — extend by adding more `buggify_knob!` lines.
    pub fn apply_buggify_knobs(&mut self) {
        self.clog_probability = crate::buggify_knob!(self.clog_probability, 0.5..1.0);
        self.partition_probability = crate::buggify_knob!(self.partition_probability, 0.3..0.8);
        self.random_close_probability =
            crate::buggify_knob!(self.random_close_probability, 0.01..0.1);
    }
}

/// Distance-based latency, one [`LatencyDistribution`] per locality class.
///
/// This is *realism*, not chaos: a cross-datacenter hop is slow on a healthy
/// day. Attach it to [`NetworkConfiguration::link_latency`] and the engine
/// classifies every IP pair through the installed locality topology
/// ([`SimWorld::set_localities`](crate::SimWorld::set_localities)), samples the
/// matching distribution once at first contact, and applies that fixed extra to
/// every delivery on the pair (it lands in the same per-pair budget as
/// [`ChaosConfiguration::max_pair_latency`], summed with it).
///
/// A pair where either endpoint has no locality gets no distance latency, so
/// plain `.processes()` runs are unaffected.
///
/// `FoundationDB` does not model this (a single global latency distribution),
/// but it is what makes cross-datacenter replication testing meaningful.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkLatencyConfig {
    /// Processes collocated on one machine (loopback).
    pub same_machine: LatencyDistribution,
    /// Different machines in the same zone (rack-local).
    pub same_zone: LatencyDistribution,
    /// Different zones in the same datacenter.
    pub same_datacenter: LatencyDistribution,
    /// Different datacenters (wide area).
    pub cross_datacenter: LatencyDistribution,
}

impl Default for LinkLatencyConfig {
    /// Rough real-world one-way link delays: microseconds on loopback, tens of
    /// milliseconds between regions.
    fn default() -> Self {
        let uniform = |start, end| LatencyDistribution::Uniform { start, end };
        Self {
            same_machine: uniform(Duration::from_micros(10), Duration::from_micros(50)),
            same_zone: uniform(Duration::from_micros(100), Duration::from_micros(500)),
            same_datacenter: uniform(Duration::from_micros(500), Duration::from_millis(2)),
            cross_datacenter: uniform(Duration::from_millis(20), Duration::from_millis(80)),
        }
    }
}

impl LinkLatencyConfig {
    /// The distribution to sample for a given locality distance.
    #[must_use]
    pub fn distribution_for(&self, class: LinkClass) -> &LatencyDistribution {
        match class {
            LinkClass::SameMachine => &self.same_machine,
            LinkClass::SameZone => &self.same_zone,
            LinkClass::SameDatacenter => &self.same_datacenter,
            LinkClass::CrossDatacenter => &self.cross_datacenter,
        }
    }
}

/// Configuration for network simulation parameters
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkConfiguration {
    /// Latency distribution for bind operations
    pub bind_latency: LatencyDistribution,
    /// Latency distribution for accept operations
    pub accept_latency: LatencyDistribution,
    /// Latency distribution for connect operations
    pub connect_latency: LatencyDistribution,
    /// Latency distribution for write operations
    pub write_latency: LatencyDistribution,

    /// Distance-based per-pair latency, resolved through the locality topology.
    ///
    /// `None` (the default) keeps every link equally fast, whatever the
    /// topology. See [`LinkLatencyConfig`].
    pub link_latency: Option<LinkLatencyConfig>,

    /// Chaos injection configuration
    pub chaos: ChaosConfiguration,
}

impl Default for NetworkConfiguration {
    fn default() -> Self {
        Self {
            bind_latency: LatencyDistribution::Uniform {
                start: Duration::from_micros(50),
                end: Duration::from_micros(150),
            },
            accept_latency: LatencyDistribution::Uniform {
                start: Duration::from_millis(1),
                end: Duration::from_millis(6),
            },
            connect_latency: LatencyDistribution::Uniform {
                start: Duration::from_millis(1),
                end: Duration::from_millis(11),
            },
            write_latency: LatencyDistribution::Uniform {
                start: Duration::from_micros(100),
                end: Duration::from_micros(600),
            },
            // Realism knob, opt-in: distance-blind by default.
            link_latency: None,
            chaos: ChaosConfiguration::default(),
        }
    }
}

/// Sample a random duration from a range
#[must_use]
pub fn sample_duration(range: &Range<Duration>) -> Duration {
    uniform_nanos(range.start, range.end)
}

/// Sample a uniform duration in `[start, end)` using the simulation RNG.
///
/// Shared by [`sample_duration`] and [`LatencyDistribution::Uniform`] so both
/// consume exactly one RNG draw with identical semantics. A degenerate range
/// (`start >= end`) returns `start` and consumes no draw (see
/// [`sim_random_range_or_default`]).
fn uniform_nanos(start: Duration, end: Duration) -> Duration {
    let start_nanos = u64::try_from(start.as_nanos()).unwrap_or(u64::MAX);
    let end_nanos = u64::try_from(end.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(sim_random_range_or_default(start_nanos..end_nanos))
}

/// A pluggable latency distribution for per-operation latency sampling.
///
/// Replaces plain uniform `Range<Duration>` sampling so simulations can exercise
/// the heavy P99 tail where timeout cascades, retry storms, and backpressure
/// collapse live. All variants sample deterministically through the simulation
/// RNG, so the same seed always yields the same sequence.
///
/// The default is [`Uniform`](Self::Uniform), which samples identically to the
/// historical [`sample_duration`] (one RNG draw, no extra draws), keeping default
/// behavior unchanged.
///
/// # References
///
/// - `Exponential` mirrors `TigerBeetle`'s `random_int_exponential` storage/network
///   delay (`packet_simulator.zig`).
/// - `Bimodal` mirrors `FoundationDB`'s `halfLatency` fast/slow split (`sim2.actor.cpp`).
#[derive(Debug, Clone, PartialEq)]
pub enum LatencyDistribution {
    /// Uniform latency in `[start, end)`. Equivalent to the historical
    /// `Range<Duration>` sampling.
    Uniform {
        /// Inclusive lower bound.
        start: Duration,
        /// Exclusive upper bound.
        end: Duration,
    },
    /// Exponential latency with a minimum floor, modelling a long tail.
    ///
    /// Samples `min + mean * (-ln(u))` with `u` drawn uniformly from `(0, 1]`,
    /// giving a mean delay of roughly `min + mean`. Note this is the additive
    /// form from the issue; `TigerBeetle` instead clamps with `max(min, exp(mean))`.
    Exponential {
        /// Minimum latency added to every sample.
        min: Duration,
        /// Mean of the exponential component (the tail scale).
        mean: Duration,
    },
    /// Bimodal latency: a fast cluster most of the time, a slow tail rarely.
    ///
    /// With probability `slow_probability` the sample is drawn uniformly from
    /// `slow_range`; otherwise from `fast_range`.
    Bimodal {
        /// Range sampled on the common, fast path.
        fast_range: Range<Duration>,
        /// Range sampled on the rare, slow path.
        slow_range: Range<Duration>,
        /// Probability in `[0, 1]` of taking the slow path.
        slow_probability: f64,
    },
}

impl Default for LatencyDistribution {
    fn default() -> Self {
        // Callers (config constructors) set real bounds per field; this
        // type-level default is only a neutral fallback.
        Self::Uniform {
            start: Duration::ZERO,
            end: Duration::ZERO,
        }
    }
}

impl LatencyDistribution {
    /// Return the `(start, end)` bounds when this is a [`Uniform`](Self::Uniform)
    /// distribution, otherwise `None`. Convenience for tests and reporting.
    #[must_use]
    pub fn uniform_bounds(&self) -> Option<(Duration, Duration)> {
        match self {
            Self::Uniform { start, end } => Some((*start, *end)),
            _ => None,
        }
    }
}

/// Sample a latency from a [`LatencyDistribution`] using the simulation RNG.
///
/// Deterministic for a given seed. The [`Uniform`](LatencyDistribution::Uniform)
/// variant consumes exactly one RNG draw (identical to [`sample_duration`]);
/// `Exponential` consumes one `f64` draw; `Bimodal` consumes one `f64` draw to
/// pick the branch plus one uniform draw.
#[must_use]
pub fn sample_latency(distribution: &LatencyDistribution) -> Duration {
    match distribution {
        LatencyDistribution::Uniform { start, end } => uniform_nanos(*start, *end),
        LatencyDistribution::Exponential { min, mean } => {
            // u in [0.0, 1.0); 1.0 - u in (0.0, 1.0] so -ln(.) in [0.0, +inf),
            // never inf or NaN. Compute in f64 seconds via `Duration`'s own API
            // to avoid lossy manual casts. A product that overflows `Duration`
            // saturates to `Duration::MAX` instead of panicking.
            let u = sim_random_f64();
            let factor = -(1.0 - u).ln();
            let extra_secs = mean.as_secs_f64() * factor;
            let extra = Duration::try_from_secs_f64(extra_secs).unwrap_or(Duration::MAX);
            min.saturating_add(extra)
        }
        LatencyDistribution::Bimodal {
            fast_range,
            slow_range,
            slow_probability,
        } => {
            // Draw the branch selector unconditionally so the RNG call-count is
            // stable regardless of which path is taken.
            if sim_random_f64() < *slow_probability {
                uniform_nanos(slow_range.start, slow_range.end)
            } else {
                uniform_nanos(fast_range.start, fast_range.end)
            }
        }
    }
}

/// Pick a per-field [`LatencyDistribution`] for chaos seeds, mixing all three
/// variants around a baseline uniform range.
///
/// One in three seeds keeps the field uniform; the others derive an exponential
/// or bimodal shape from the same baseline so the mean stays comparable. Draws
/// from the simulation RNG, so it is deterministic per seed.
pub(crate) fn random_latency_for_seed(uniform: Range<Duration>) -> LatencyDistribution {
    match sim_random_range(0..3) {
        0 => LatencyDistribution::Uniform {
            start: uniform.start,
            end: uniform.end,
        },
        1 => LatencyDistribution::Exponential {
            min: uniform.start,
            // Mean tail scale of one baseline width above the floor.
            mean: uniform.end.saturating_sub(uniform.start),
        },
        _ => {
            // Slow path is one decade beyond the baseline upper bound.
            let slow_start = uniform.end;
            let slow_end = uniform.end.saturating_mul(10);
            LatencyDistribution::Bimodal {
                fast_range: uniform,
                slow_range: slow_start..slow_end,
                // 0.1% .. 1% slow tail.
                slow_probability: f64::from(sim_random_range(1..10)) / 1000.0,
            }
        }
    }
}

impl NetworkConfiguration {
    /// Create a new network configuration with default settings
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a randomized network configuration for chaos testing
    #[must_use]
    pub fn random_for_seed() -> Self {
        Self {
            bind_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(10..200))
                    ..Duration::from_micros(sim_random_range(50..300)),
            ),
            accept_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(1000..10_000))
                    ..Duration::from_micros(sim_random_range(5000..15_000)),
            ),
            connect_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(1000..50_000))
                    ..Duration::from_micros(sim_random_range(10_000..100_000)),
            ),
            write_latency: random_latency_for_seed(
                Duration::from_micros(sim_random_range(50..1000))
                    ..Duration::from_micros(sim_random_range(200..2000)),
            ),
            // Distance latency models the deployment, not the seed, so chaos
            // runs leave it to the caller (and draw no RNG for it).
            link_latency: None,
            chaos: ChaosConfiguration::random_for_seed(),
        }
    }

    /// Create a swarm-testing network configuration for seed-based testing.
    ///
    /// Identical to [`random_for_seed`](Self::random_for_seed) for the baseline
    /// latencies, but the embedded [`ChaosConfiguration`] enables only a random
    /// *subset* of fault families per seed. See
    /// [`ChaosConfiguration::swarm_for_seed`].
    #[must_use]
    pub fn swarm_for_seed() -> Self {
        let mut config = Self::random_for_seed();
        config.chaos.apply_swarm_mask();
        config
    }

    /// Create a configuration optimized for fast local testing
    #[must_use]
    pub fn fast_local() -> Self {
        let one_us = Duration::from_micros(1);
        let ten_us = Duration::from_micros(10);
        let uniform = |start, end| LatencyDistribution::Uniform { start, end };
        Self {
            bind_latency: uniform(one_us, one_us),
            accept_latency: uniform(ten_us, ten_us),
            connect_latency: uniform(ten_us, ten_us),
            write_latency: uniform(one_us, one_us),
            link_latency: None,
            chaos: ChaosConfiguration::disabled(),
        }
    }
}

#[cfg(test)]
mod swarm_tests {
    use super::{ChaosConfiguration, ConnectFailureMode, NetworkConfiguration};
    use crate::sim::rng::{reset_sim_rng, set_config_seed, set_sim_seed};

    /// The on/off state of each swarmed fault family, in mask order.
    fn enabled_families(chaos: &ChaosConfiguration) -> [bool; 7] {
        [
            chaos.clog_probability > 0.0,
            chaos.partition_probability > 0.0,
            chaos.bit_flip_probability > 0.0,
            chaos.random_close_probability > 0.0,
            chaos.connect_failure_mode != ConnectFailureMode::Disabled,
            chaos.clock_drift_enabled,
            chaos.buggified_delay_enabled,
        ]
    }

    /// Build a swarm config the way the runner does: both streams seeded per iteration.
    fn swarm_for(seed: u64) -> NetworkConfiguration {
        reset_sim_rng();
        set_sim_seed(seed);
        set_config_seed(seed);
        NetworkConfiguration::swarm_for_seed()
    }

    #[test]
    fn swarm_subset_is_deterministic_per_seed() {
        for seed in [0_u64, 1, 42, 12_345] {
            let first = enabled_families(&swarm_for(seed).chaos);
            let second = enabled_families(&swarm_for(seed).chaos);
            assert_eq!(
                first, second,
                "swarm subset must be reproducible for seed {seed}"
            );
        }
    }

    #[test]
    fn swarm_reaches_all_off_and_mixed_subsets() {
        let mut saw_all_off = false;
        let mut saw_mixed = false;

        for seed in 0..1000_u64 {
            let families = enabled_families(&swarm_for(seed).chaos);
            let on = families.iter().filter(|&&e| e).count();
            if on == 0 {
                saw_all_off = true;
            }
            if on > 0 && on < families.len() {
                saw_mixed = true;
            }
            if saw_all_off && saw_mixed {
                break;
            }
        }

        assert!(
            saw_all_off,
            "no seed in 0..1000 produced the all-off subset"
        );
        assert!(saw_mixed, "no seed in 0..1000 produced a mixed subset");
    }

    #[test]
    fn swarm_all_off_seed_has_zero_fault_probabilities() {
        // Find a seed whose subset is entirely off, then assert every family is inert.
        let seed = (0..1000_u64)
            .find(|&s| enabled_families(&swarm_for(s).chaos).iter().all(|&e| !e))
            .expect("expected an all-off seed within 0..1000");

        let chaos = swarm_for(seed).chaos;
        assert_zero(chaos.clog_probability);
        assert_zero(chaos.partition_probability);
        assert_zero(chaos.bit_flip_probability);
        assert_zero(chaos.random_close_probability);
        assert_eq!(chaos.connect_failure_mode, ConnectFailureMode::Disabled);
        assert_zero(chaos.connect_failure_probability);
        assert!(!chaos.clock_drift_enabled);
        assert!(!chaos.buggified_delay_enabled);
    }

    /// Assert an f64 is exactly `+0.0` (bit-exact, avoiding the float-cmp lint).
    fn assert_zero(value: f64) {
        assert_eq!(
            value.to_bits(),
            0.0_f64.to_bits(),
            "expected 0.0, got {value}"
        );
    }
}

#[cfg(test)]
mod partition_strategy_tests {
    use super::{ChaosConfiguration, PartitionStrategy};
    use crate::sim::rng::{reset_sim_rng, set_sim_seed};
    use std::collections::BTreeSet;

    fn strategy_for(seed: u64) -> PartitionStrategy {
        reset_sim_rng();
        set_sim_seed(seed);
        ChaosConfiguration::random_for_seed().partition_strategy
    }

    #[test]
    fn random_for_seed_reaches_every_strategy() {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for seed in 0..500_u64 {
            seen.insert(format!("{:?}", strategy_for(seed)));
        }
        for expected in [
            PartitionStrategy::Random,
            PartitionStrategy::UniformSize,
            PartitionStrategy::IsolateSingle,
            PartitionStrategy::IsolateZone,
            PartitionStrategy::IsolateDatacenter,
            PartitionStrategy::AsymmetricSend,
            PartitionStrategy::AsymmetricRecv,
        ] {
            assert!(
                seen.contains(&format!("{expected:?}")),
                "no seed in 0..500 selected {expected:?}"
            );
        }
    }

    #[test]
    fn strategy_selection_is_reproducible_per_seed() {
        for seed in [0_u64, 1, 42, 12_345] {
            assert_eq!(strategy_for(seed), strategy_for(seed));
        }
    }
}

#[cfg(test)]
mod latency_distribution_tests {
    use super::{LatencyDistribution, NetworkConfiguration, sample_duration, sample_latency};
    use crate::sim::rng::set_sim_seed;
    use crate::storage::StorageConfiguration;
    use std::time::Duration;

    /// Collect `n` samples from a distribution under a fresh seed.
    fn samples(seed: u64, dist: &LatencyDistribution, n: usize) -> Vec<Duration> {
        set_sim_seed(seed);
        (0..n).map(|_| sample_latency(dist)).collect()
    }

    /// The 99th percentile of a sample set, by sorted index.
    fn p99(mut values: Vec<Duration>) -> Duration {
        values.sort_unstable();
        let idx = ((values.len() * 99) / 100).min(values.len() - 1);
        values[idx]
    }

    #[test]
    fn uniform_matches_sample_duration_byte_for_byte() {
        // The Uniform variant must consume exactly the same RNG as the legacy
        // `sample_duration`: identical value AND identical resulting RNG state
        // (one draw, no extra). Two interleaved draws prove both.
        let start = Duration::from_micros(100);
        let end = Duration::from_micros(600);
        let dist = LatencyDistribution::Uniform { start, end };
        let range = start..end;

        set_sim_seed(7);
        let a1 = sample_latency(&dist);
        let a2 = sample_duration(&range);

        set_sim_seed(7);
        let b1 = sample_duration(&range);
        let b2 = sample_duration(&range);

        assert_eq!((a1, a2), (b1, b2));
    }

    #[test]
    fn each_variant_is_deterministic_per_seed() {
        let variants = [
            LatencyDistribution::Uniform {
                start: Duration::from_micros(10),
                end: Duration::from_micros(60),
            },
            LatencyDistribution::Exponential {
                min: Duration::from_micros(10),
                mean: Duration::from_micros(100),
            },
            LatencyDistribution::Bimodal {
                fast_range: Duration::from_millis(1)..Duration::from_millis(2),
                slow_range: Duration::from_millis(50)..Duration::from_millis(100),
                slow_probability: 0.05,
            },
        ];
        for dist in &variants {
            let first = samples(42, dist, 64);
            let second = samples(42, dist, 64);
            assert_eq!(first, second, "distribution not deterministic: {dist:?}");
        }
    }

    #[test]
    fn default_configs_are_all_uniform() {
        let net = NetworkConfiguration::default();
        for dist in [
            &net.bind_latency,
            &net.accept_latency,
            &net.connect_latency,
            &net.write_latency,
        ] {
            assert!(
                dist.uniform_bounds().is_some(),
                "network default not uniform: {dist:?}"
            );
        }
        let storage = StorageConfiguration::default();
        for dist in [
            &storage.read_latency,
            &storage.write_latency,
            &storage.sync_latency,
        ] {
            assert!(
                dist.uniform_bounds().is_some(),
                "storage default not uniform: {dist:?}"
            );
        }
    }

    #[test]
    fn exponential_has_heavier_tail_than_uniform_at_equal_mean() {
        // Uniform [0, 2ms) and Exponential{min: 0, mean: 1ms} share a 1ms mean,
        // but the exponential's tail pushes its p99 well above the uniform's.
        let uniform = LatencyDistribution::Uniform {
            start: Duration::ZERO,
            end: Duration::from_millis(2),
        };
        let exponential = LatencyDistribution::Exponential {
            min: Duration::ZERO,
            mean: Duration::from_millis(1),
        };
        let uni_p99 = p99(samples(123, &uniform, 10_000));
        let exp_p99 = p99(samples(123, &exponential, 10_000));
        assert!(
            exp_p99 > uni_p99,
            "exponential p99 {exp_p99:?} should exceed uniform p99 {uni_p99:?}"
        );
    }

    #[test]
    fn bimodal_shows_fast_cluster_and_slow_tail() {
        let fast = Duration::from_millis(1)..Duration::from_millis(2);
        let slow = Duration::from_millis(50)..Duration::from_millis(100);
        let dist = LatencyDistribution::Bimodal {
            fast_range: fast.clone(),
            slow_range: slow.clone(),
            slow_probability: 0.05,
        };
        let values = samples(99, &dist, 10_000);
        let fast_count = values.iter().filter(|d| fast.contains(d)).count();
        let slow_count = values.iter().filter(|d| slow.contains(d)).count();
        assert!(
            fast_count > 8_000,
            "expected a dominant fast cluster, got {fast_count}"
        );
        assert!(slow_count > 0, "expected a non-empty slow tail");
        assert_eq!(fast_count + slow_count, values.len());
    }

    #[test]
    fn exponential_with_zero_mean_returns_min() {
        let dist = LatencyDistribution::Exponential {
            min: Duration::from_micros(42),
            mean: Duration::ZERO,
        };
        set_sim_seed(5);
        for _ in 0..100 {
            assert_eq!(sample_latency(&dist), Duration::from_micros(42));
        }
    }

    #[test]
    fn bimodal_probability_bounds_select_expected_range() {
        let fast = Duration::from_millis(1)..Duration::from_millis(2);
        let slow = Duration::from_millis(50)..Duration::from_millis(100);
        let never_slow = LatencyDistribution::Bimodal {
            fast_range: fast.clone(),
            slow_range: slow.clone(),
            slow_probability: 0.0,
        };
        let always_slow = LatencyDistribution::Bimodal {
            fast_range: fast.clone(),
            slow_range: slow.clone(),
            slow_probability: 1.0,
        };
        for d in samples(1, &never_slow, 500) {
            assert!(fast.contains(&d), "slow_probability 0.0 produced {d:?}");
        }
        for d in samples(2, &always_slow, 500) {
            assert!(slow.contains(&d), "slow_probability 1.0 produced {d:?}");
        }
    }

    #[test]
    fn exponential_saturates_instead_of_panicking() {
        // A mean near the Duration ceiling can overflow when scaled by the tail
        // factor; sampling must saturate, never panic.
        let dist = LatencyDistribution::Exponential {
            min: Duration::from_secs(1),
            mean: Duration::from_secs(u64::MAX / 2),
        };
        set_sim_seed(3);
        for _ in 0..1_000 {
            let sampled = sample_latency(&dist);
            assert!(sampled >= Duration::from_secs(1));
        }
    }
}
