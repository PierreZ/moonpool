//! Transport operation alphabet for FDB-style randomized simulation testing.
//!
//! Defines the complete set of operations (Normal, Adversarial, Nemesis)
//! and weighted random selection for chaos testing.

use moonpool_sim::RandomProvider;

/// All operations a transport workload can perform.
///
/// Organized into three tiers following FDB's alphabet pattern:
/// - **Normal (70%)**: Happy-path operations
/// - **Adversarial (20%)**: Edge-case payloads and targets
/// - **Nemesis (10%)**: Destructive operations that break invariants temporarily
#[derive(Debug, Clone)]
pub enum TransportOp {
    // ---- Normal (70%) ----
    /// Send a reliable message to the peer.
    SendReliable { seq_id: u64, payload_size: usize },
    /// Send an unreliable message to the peer.
    SendUnreliable { seq_id: u64, payload_size: usize },
    /// Send an RPC request to the peer.
    SendRpc { request_id: u64 },
    /// Small delay between operations.
    SmallDelay,

    // ---- Adversarial (20%) ----
    /// Send a zero-length payload (edge case).
    SendEmptyPayload { reliable: bool },
    /// Send a maximum-size payload.
    SendMaxSizePayload { reliable: bool },
    /// Send to an endpoint that doesn't exist.
    SendToUnknownEndpoint,

    // ---- Nemesis (10%) ----
    /// Unregister an endpoint (causes undelivered messages).
    UnregisterEndpoint,
    /// Re-register an endpoint after unregistering.
    ReregisterEndpoint,
    /// Drop an RPC promise without responding (triggers BrokenPromise).
    DropRpcPromise,
}

/// Weights controlling the tier probability distribution.
#[derive(Debug, Clone)]
pub struct AlphabetWeights {
    /// Weight for normal operations (default: 70).
    pub normal: u32,
    /// Weight for adversarial operations (default: 20).
    pub adversarial: u32,
    /// Weight for nemesis operations (default: 10).
    pub nemesis: u32,
}

impl Default for AlphabetWeights {
    fn default() -> Self {
        Self {
            normal: 70,
            adversarial: 20,
            nemesis: 10,
        }
    }
}

impl AlphabetWeights {
    /// Happy-path only weights (no adversarial or nemesis).
    pub fn happy_path() -> Self {
        Self {
            normal: 100,
            adversarial: 0,
            nemesis: 0,
        }
    }

    /// Adversarial-heavy weights for edge-case testing.
    pub fn adversarial_heavy() -> Self {
        Self {
            normal: 40,
            adversarial: 50,
            nemesis: 10,
        }
    }

    /// Nemesis-heavy weights for destructive testing.
    pub fn nemesis_heavy() -> Self {
        Self {
            normal: 40,
            adversarial: 20,
            nemesis: 40,
        }
    }

    fn total(&self) -> u32 {
        self.normal + self.adversarial + self.nemesis
    }
}

/// Mutable counters for generating unique sequence IDs.
pub struct OpCounters {
    /// Next reliable message sequence ID.
    pub next_reliable_seq: u64,
    /// Next unreliable message sequence ID.
    pub next_unreliable_seq: u64,
    /// Next RPC request ID.
    pub next_rpc_id: u64,
    /// Whether an endpoint is currently unregistered (for nemesis ops).
    pub endpoint_unregistered: bool,
}

impl OpCounters {
    /// Create new counters starting from zero.
    pub fn new() -> Self {
        Self {
            next_reliable_seq: 0,
            next_unreliable_seq: 10_000,
            next_rpc_id: 0,
            endpoint_unregistered: false,
        }
    }
}

/// Pick a random transport operation based on the alphabet weights.
///
/// Uses the random provider for deterministic selection.
pub fn pick_operation<R: RandomProvider>(
    random: &R,
    weights: &AlphabetWeights,
    counters: &mut OpCounters,
) -> TransportOp {
    let total = weights.total();
    if total == 0 {
        return TransportOp::SmallDelay;
    }
    let roll = random.random_range(0..total);

    if roll < weights.normal {
        pick_normal(random, counters)
    } else if roll < weights.normal + weights.adversarial {
        pick_adversarial(random)
    } else {
        pick_nemesis(random, counters)
    }
}

/// Pick a random normal operation.
fn pick_normal<R: RandomProvider>(random: &R, counters: &mut OpCounters) -> TransportOp {
    let choice = random.random_range(0..4u32);
    match choice {
        0 => {
            let seq_id = counters.next_reliable_seq;
            counters.next_reliable_seq += 1;
            let payload_size = random.random_range(1..256u32) as usize;
            TransportOp::SendReliable {
                seq_id,
                payload_size,
            }
        }
        1 => {
            let seq_id = counters.next_unreliable_seq;
            counters.next_unreliable_seq += 1;
            let payload_size = random.random_range(1..256u32) as usize;
            TransportOp::SendUnreliable {
                seq_id,
                payload_size,
            }
        }
        2 => {
            let request_id = counters.next_rpc_id;
            counters.next_rpc_id += 1;
            TransportOp::SendRpc { request_id }
        }
        _ => TransportOp::SmallDelay,
    }
}

/// Pick a random adversarial operation.
fn pick_adversarial<R: RandomProvider>(random: &R) -> TransportOp {
    let choice = random.random_range(0..3u32);
    match choice {
        0 => TransportOp::SendEmptyPayload {
            reliable: random.random_range(0..2u32) == 0,
        },
        1 => TransportOp::SendMaxSizePayload {
            reliable: random.random_range(0..2u32) == 0,
        },
        _ => TransportOp::SendToUnknownEndpoint,
    }
}

/// Pick a random nemesis operation.
fn pick_nemesis<R: RandomProvider>(random: &R, counters: &mut OpCounters) -> TransportOp {
    let choice = random.random_range(0..3u32);
    match choice {
        0 => {
            if !counters.endpoint_unregistered {
                counters.endpoint_unregistered = true;
                TransportOp::UnregisterEndpoint
            } else {
                // Already unregistered, re-register instead
                counters.endpoint_unregistered = false;
                TransportOp::ReregisterEndpoint
            }
        }
        1 => {
            if counters.endpoint_unregistered {
                counters.endpoint_unregistered = false;
                TransportOp::ReregisterEndpoint
            } else {
                TransportOp::DropRpcPromise
            }
        }
        _ => TransportOp::DropRpcPromise,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonpool_sim::SimRandomProvider;

    #[test]
    fn test_default_weights() {
        let w = AlphabetWeights::default();
        assert_eq!(w.total(), 100);
    }

    #[test]
    fn test_pick_operation_deterministic() {
        let random = SimRandomProvider::new(42);
        let weights = AlphabetWeights::default();
        let mut counters = OpCounters::new();

        let mut ops1 = Vec::new();
        for _ in 0..20 {
            ops1.push(format!(
                "{:?}",
                pick_operation(&random, &weights, &mut counters)
            ));
        }

        // Same seed should produce same sequence
        let random2 = SimRandomProvider::new(42);
        let mut counters2 = OpCounters::new();
        let mut ops2 = Vec::new();
        for _ in 0..20 {
            ops2.push(format!(
                "{:?}",
                pick_operation(&random2, &weights, &mut counters2)
            ));
        }

        assert_eq!(ops1, ops2);
    }

    #[test]
    fn test_happy_path_only_normal() {
        let random = SimRandomProvider::new(123);
        let weights = AlphabetWeights::happy_path();
        let mut counters = OpCounters::new();

        for _ in 0..100 {
            let op = pick_operation(&random, &weights, &mut counters);
            match op {
                TransportOp::SendReliable { .. }
                | TransportOp::SendUnreliable { .. }
                | TransportOp::SendRpc { .. }
                | TransportOp::SmallDelay => {}
                other => panic!("Expected normal op, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_counters_increment() {
        let random = SimRandomProvider::new(42);
        let weights = AlphabetWeights::happy_path();
        let mut counters = OpCounters::new();

        for _ in 0..100 {
            let _ = pick_operation(&random, &weights, &mut counters);
        }

        // Should have generated some of each type
        assert!(counters.next_reliable_seq > 0);
        assert!(counters.next_unreliable_seq > 10_000);
        assert!(counters.next_rpc_id > 0);
    }
}
