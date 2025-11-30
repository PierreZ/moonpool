//! Operation alphabet for randomized simulation testing.
//!
//! Defines the set of operations workloads can perform and
//! weighted random selection for chaos testing.

use moonpool_foundation::RandomProvider;

/// Operations a client workload can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientOp {
    /// Send a reliable message
    SendReliable { seq_id: u64, payload_size: usize },
    /// Send an unreliable message
    SendUnreliable { seq_id: u64, payload_size: usize },
    /// Small delay between operations
    SmallDelay,
}

/// Operations a server workload can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerOp {
    /// Try to receive a message (non-blocking)
    TryReceive,
    /// Wait for a message with timeout
    Receive,
    /// Small delay between operations
    SmallDelay,
}

/// Weights for client operation selection.
#[derive(Debug, Clone)]
pub struct ClientOpWeights {
    pub send_reliable: u32,
    pub send_unreliable: u32,
    pub small_delay: u32,
}

impl Default for ClientOpWeights {
    fn default() -> Self {
        Self {
            send_reliable: 40,
            send_unreliable: 40,
            small_delay: 20,
        }
    }
}

impl ClientOpWeights {
    /// Weights focused on reliable delivery testing.
    pub fn reliable_focused() -> Self {
        Self {
            send_reliable: 80,
            send_unreliable: 10,
            small_delay: 10,
        }
    }

    /// Weights focused on unreliable delivery testing.
    pub fn unreliable_focused() -> Self {
        Self {
            send_reliable: 10,
            send_unreliable: 80,
            small_delay: 10,
        }
    }

    /// Weights for mixed testing with more delays.
    pub fn mixed_with_delays() -> Self {
        Self {
            send_reliable: 30,
            send_unreliable: 30,
            small_delay: 40,
        }
    }

    fn total_weight(&self) -> u32 {
        self.send_reliable + self.send_unreliable + self.small_delay
    }
}

/// Weights for server operation selection.
#[derive(Debug, Clone)]
pub struct ServerOpWeights {
    pub try_receive: u32,
    pub receive: u32,
    pub small_delay: u32,
}

impl Default for ServerOpWeights {
    fn default() -> Self {
        Self {
            try_receive: 30,
            receive: 50,
            small_delay: 20,
        }
    }
}

impl ServerOpWeights {
    fn total_weight(&self) -> u32 {
        self.try_receive + self.receive + self.small_delay
    }
}

/// Generate a random client operation.
pub fn generate_client_op<R: RandomProvider>(
    random: &R,
    next_reliable_seq: &mut u64,
    next_unreliable_seq: &mut u64,
    weights: &ClientOpWeights,
) -> ClientOp {
    let total = weights.total_weight();
    let choice = random.random_range(0..total);

    let mut cumulative = 0;

    cumulative += weights.send_reliable;
    if choice < cumulative {
        let seq_id = *next_reliable_seq;
        *next_reliable_seq += 1;
        // Random payload size 0-256 bytes
        let payload_size = random.random_range(0..256) as usize;
        return ClientOp::SendReliable {
            seq_id,
            payload_size,
        };
    }

    cumulative += weights.send_unreliable;
    if choice < cumulative {
        let seq_id = *next_unreliable_seq;
        *next_unreliable_seq += 1;
        let payload_size = random.random_range(0..256) as usize;
        return ClientOp::SendUnreliable {
            seq_id,
            payload_size,
        };
    }

    ClientOp::SmallDelay
}

/// Generate a random server operation.
pub fn generate_server_op<R: RandomProvider>(random: &R, weights: &ServerOpWeights) -> ServerOp {
    let total = weights.total_weight();
    let choice = random.random_range(0..total);

    let mut cumulative = 0;

    cumulative += weights.try_receive;
    if choice < cumulative {
        return ServerOp::TryReceive;
    }

    cumulative += weights.receive;
    if choice < cumulative {
        return ServerOp::Receive;
    }

    ServerOp::SmallDelay
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonpool_foundation::SimRandomProvider;

    #[test]
    fn test_client_op_weights_default() {
        let weights = ClientOpWeights::default();
        assert_eq!(weights.send_reliable, 40);
        assert_eq!(weights.send_unreliable, 40);
        assert_eq!(weights.small_delay, 20);
        assert_eq!(weights.total_weight(), 100);
    }

    #[test]
    fn test_client_op_weights_presets() {
        let reliable = ClientOpWeights::reliable_focused();
        assert_eq!(reliable.send_reliable, 80);

        let unreliable = ClientOpWeights::unreliable_focused();
        assert_eq!(unreliable.send_unreliable, 80);

        let mixed = ClientOpWeights::mixed_with_delays();
        assert_eq!(mixed.small_delay, 40);
    }

    #[test]
    fn test_generate_client_op_produces_valid_ops() {
        let random = SimRandomProvider::new(42);
        let weights = ClientOpWeights::default();
        let mut rel_seq = 0;
        let mut unrel_seq = 10000;

        // Generate many ops and verify they're all valid
        for _ in 0..100 {
            let op = generate_client_op(&random, &mut rel_seq, &mut unrel_seq, &weights);
            match op {
                ClientOp::SendReliable {
                    seq_id,
                    payload_size,
                } => {
                    assert!(seq_id < 10000); // Reliable should be in low range
                    assert!(payload_size < 256);
                }
                ClientOp::SendUnreliable {
                    seq_id,
                    payload_size,
                } => {
                    assert!(seq_id >= 10000); // Unreliable starts at 10000
                    assert!(payload_size < 256);
                }
                ClientOp::SmallDelay => {}
            }
        }
    }

    #[test]
    fn test_generate_server_op_produces_valid_ops() {
        let random = SimRandomProvider::new(42);
        let weights = ServerOpWeights::default();

        for _ in 0..100 {
            let op = generate_server_op(&random, &weights);
            assert!(matches!(
                op,
                ServerOp::TryReceive | ServerOp::Receive | ServerOp::SmallDelay
            ));
        }
    }

    #[test]
    fn test_deterministic_generation() {
        // Same seed produces same sequence when reset between runs
        let weights = ClientOpWeights::default();

        // First run with seed 123
        let random = SimRandomProvider::new(123);
        let mut rel_seq = 0;
        let mut unrel_seq = 1000;
        let mut ops1: Vec<ClientOp> = Vec::new();
        for _ in 0..10 {
            ops1.push(generate_client_op(
                &random,
                &mut rel_seq,
                &mut unrel_seq,
                &weights,
            ));
        }

        // Second run with same seed 123 - should produce same ops
        let random = SimRandomProvider::new(123); // This resets the RNG
        let mut rel_seq = 0;
        let mut unrel_seq = 1000;
        let mut ops2: Vec<ClientOp> = Vec::new();
        for _ in 0..10 {
            ops2.push(generate_client_op(
                &random,
                &mut rel_seq,
                &mut unrel_seq,
                &weights,
            ));
        }

        assert_eq!(ops1, ops2);
    }
}
