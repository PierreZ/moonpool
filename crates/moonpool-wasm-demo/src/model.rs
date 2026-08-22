use serde::Serialize;

/// How a shot (one leg of a round trip) resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The leg was delivered over the network.
    Delivered,
    /// The request was dropped or timed out under chaos.
    Dropped,
}

/// One message crossing the simulated network.
#[derive(Debug, Clone, Serialize)]
pub struct Shot {
    /// Request sequence number this leg belongs to.
    pub seq: u64,
    /// Node that sent this message (0 = A/client, 1 = B/server).
    pub from: u8,
    /// Node the message travels to (0 = A/client, 1 = B/server).
    pub to: u8,
    /// Simulated time the message left `from`, in milliseconds.
    pub depart_ms: u64,
    /// Simulated time the message reached `to`, in milliseconds.
    pub arrive_ms: u64,
    /// In-flight latency, in milliseconds.
    pub latency_ms: u64,
    /// Whether this leg was delivered or dropped.
    pub outcome: Outcome,
}

/// The result of one seeded run, including the animation timeline.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    /// The seed this run used.
    pub seed: u64,
    /// Number of ping requests observed.
    pub requests: u32,
    /// Every message leg exchanged, in time order.
    pub shots: Vec<Shot>,
    /// Round trips that completed successfully.
    pub delivered: u32,
    /// Requests dropped or timed out under chaos.
    pub dropped: u32,
    /// Network faults the simulator injected.
    pub faults: u32,
    /// Slowest successful round trip, in simulated milliseconds.
    pub longest_rtt_ms: u64,
    /// Total simulated time elapsed, in milliseconds.
    pub sim_duration_ms: u64,
}

impl RunResult {
    pub(crate) fn empty(seed: u64) -> Self {
        Self {
            seed,
            requests: 0,
            shots: Vec::new(),
            delivered: 0,
            dropped: 0,
            faults: 0,
            longest_rtt_ms: 0,
            sim_duration_ms: 0,
        }
    }
}
