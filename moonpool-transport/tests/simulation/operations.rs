//! Operation alphabet for randomized simulation testing.
//!
//! Defines the set of operations workloads can perform and
//! weighted random selection for chaos testing.

use moonpool_sim::RandomProvider;

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

// ============================================================================
// RPC Operations (Phase 12B)
// ============================================================================

/// Operations an RPC client can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcClientOp {
    /// Send an RPC request
    SendRequest { request_id: u64 },
    /// Await a pending RPC response
    AwaitResponse { request_id: u64 },
    /// Await a pending RPC response with timeout
    AwaitWithTimeout { request_id: u64, timeout_ms: u64 },
    /// Small delay between operations
    SmallDelay,
}

/// Operations an RPC server can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcServerOp {
    /// Try to receive a request (non-blocking)
    TryReceiveRequest,
    /// Send a successful response for a pending request
    SendResponse { request_id: u64 },
    /// Drop a promise without responding (triggers BrokenPromise)
    DropPromise { request_id: u64 },
    /// Small delay between operations
    SmallDelay,
}

/// Weights for RPC client operation selection.
#[derive(Debug, Clone)]
pub struct RpcClientOpWeights {
    pub send_request: u32,
    pub await_response: u32,
    pub await_with_timeout: u32,
    pub small_delay: u32,
}

impl Default for RpcClientOpWeights {
    fn default() -> Self {
        Self {
            send_request: 35,
            await_response: 35,
            await_with_timeout: 15,
            small_delay: 15,
        }
    }
}

impl RpcClientOpWeights {
    /// Weights focused on sending many requests (high concurrency).
    pub fn high_concurrency() -> Self {
        Self {
            send_request: 60,
            await_response: 15,
            await_with_timeout: 10,
            small_delay: 15,
        }
    }

    /// Weights focused on completing requests (low concurrency).
    pub fn low_concurrency() -> Self {
        Self {
            send_request: 15,
            await_response: 50,
            await_with_timeout: 20,
            small_delay: 15,
        }
    }

    /// Weights focused on timeout path coverage.
    pub fn timeout_focused() -> Self {
        Self {
            send_request: 30,
            await_response: 10,
            await_with_timeout: 45,
            small_delay: 15,
        }
    }

    fn total_weight(&self) -> u32 {
        self.send_request + self.await_response + self.await_with_timeout + self.small_delay
    }
}

/// Weights for RPC server operation selection.
#[derive(Debug, Clone)]
pub struct RpcServerOpWeights {
    pub try_receive: u32,
    pub send_response: u32,
    pub drop_promise: u32,
    pub small_delay: u32,
}

impl Default for RpcServerOpWeights {
    fn default() -> Self {
        Self {
            try_receive: 30,
            send_response: 50,
            drop_promise: 10,
            small_delay: 10,
        }
    }
}

impl RpcServerOpWeights {
    /// Weights for happy path (no broken promises).
    pub fn happy_path() -> Self {
        Self {
            try_receive: 30,
            send_response: 60,
            drop_promise: 0,
            small_delay: 10,
        }
    }

    /// Weights focused on broken promise testing.
    pub fn broken_promise_focused() -> Self {
        Self {
            try_receive: 30,
            send_response: 30,
            drop_promise: 30,
            small_delay: 10,
        }
    }

    fn total_weight(&self) -> u32 {
        self.try_receive + self.send_response + self.drop_promise + self.small_delay
    }
}

/// Generate a random RPC client operation.
pub fn generate_rpc_client_op<R: RandomProvider>(
    random: &R,
    next_request_id: &mut u64,
    pending_requests: &[u64],
    weights: &RpcClientOpWeights,
) -> RpcClientOp {
    let total = weights.total_weight();
    let choice = random.random_range(0..total);

    let mut cumulative = 0;

    cumulative += weights.send_request;
    if choice < cumulative {
        let request_id = *next_request_id;
        *next_request_id += 1;
        return RpcClientOp::SendRequest { request_id };
    }

    cumulative += weights.await_response;
    if choice < cumulative && !pending_requests.is_empty() {
        // Pick a random pending request to await
        let idx = random.random_range(0..pending_requests.len() as u32) as usize;
        return RpcClientOp::AwaitResponse {
            request_id: pending_requests[idx],
        };
    }

    cumulative += weights.await_with_timeout;
    if choice < cumulative && !pending_requests.is_empty() {
        // Pick a random pending request to await with timeout
        let idx = random.random_range(0..pending_requests.len() as u32) as usize;
        // Timeout between 1-100ms
        let timeout_ms = random.random_range(1..100) as u64;
        return RpcClientOp::AwaitWithTimeout {
            request_id: pending_requests[idx],
            timeout_ms,
        };
    }

    RpcClientOp::SmallDelay
}

/// Generate a random RPC server operation.
pub fn generate_rpc_server_op<R: RandomProvider>(
    random: &R,
    pending_promises: &[u64],
    weights: &RpcServerOpWeights,
) -> RpcServerOp {
    let total = weights.total_weight();
    let choice = random.random_range(0..total);

    let mut cumulative = 0;

    cumulative += weights.try_receive;
    if choice < cumulative {
        return RpcServerOp::TryReceiveRequest;
    }

    cumulative += weights.send_response;
    if choice < cumulative && !pending_promises.is_empty() {
        let idx = random.random_range(0..pending_promises.len() as u32) as usize;
        return RpcServerOp::SendResponse {
            request_id: pending_promises[idx],
        };
    }

    cumulative += weights.drop_promise;
    if choice < cumulative && !pending_promises.is_empty() {
        let idx = random.random_range(0..pending_promises.len() as u32) as usize;
        return RpcServerOp::DropPromise {
            request_id: pending_promises[idx],
        };
    }

    RpcServerOp::SmallDelay
}

#[cfg(test)]
mod tests {
    use super::*;
    use moonpool_sim::SimRandomProvider;

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

    // ========================================================================
    // RPC Operations Tests
    // ========================================================================

    #[test]
    fn test_rpc_client_op_weights_default() {
        let weights = RpcClientOpWeights::default();
        assert_eq!(weights.send_request, 35);
        assert_eq!(weights.await_response, 35);
        assert_eq!(weights.await_with_timeout, 15);
        assert_eq!(weights.small_delay, 15);
        assert_eq!(weights.total_weight(), 100);
    }

    #[test]
    fn test_rpc_server_op_weights_presets() {
        let happy = RpcServerOpWeights::happy_path();
        assert_eq!(happy.drop_promise, 0);

        let broken = RpcServerOpWeights::broken_promise_focused();
        assert_eq!(broken.drop_promise, 30);
    }

    #[test]
    fn test_generate_rpc_client_op_produces_valid_ops() {
        let random = SimRandomProvider::new(42);
        let weights = RpcClientOpWeights::default();
        let mut next_id = 0;
        let pending = vec![1, 2, 3];

        for _ in 0..100 {
            let op = generate_rpc_client_op(&random, &mut next_id, &pending, &weights);
            match op {
                RpcClientOp::SendRequest { request_id } => {
                    assert!(request_id < 1000);
                }
                RpcClientOp::AwaitResponse { request_id } => {
                    assert!(pending.contains(&request_id));
                }
                RpcClientOp::AwaitWithTimeout {
                    request_id,
                    timeout_ms,
                } => {
                    assert!(pending.contains(&request_id));
                    assert!(timeout_ms >= 1 && timeout_ms < 100);
                }
                RpcClientOp::SmallDelay => {}
            }
        }
    }

    #[test]
    fn test_generate_rpc_server_op_produces_valid_ops() {
        let random = SimRandomProvider::new(42);
        let weights = RpcServerOpWeights::default();
        let pending = vec![10, 20, 30];

        for _ in 0..100 {
            let op = generate_rpc_server_op(&random, &pending, &weights);
            match op {
                RpcServerOp::TryReceiveRequest => {}
                RpcServerOp::SendResponse { request_id } => {
                    assert!(pending.contains(&request_id));
                }
                RpcServerOp::DropPromise { request_id } => {
                    assert!(pending.contains(&request_id));
                }
                RpcServerOp::SmallDelay => {}
            }
        }
    }

    #[test]
    fn test_rpc_server_op_no_promises_fallback() {
        // With no pending promises, send_response/drop_promise fall back to delay
        let random = SimRandomProvider::new(42);
        let weights = RpcServerOpWeights {
            try_receive: 0,
            send_response: 50,
            drop_promise: 50,
            small_delay: 0,
        };
        let empty: Vec<u64> = vec![];

        for _ in 0..100 {
            let op = generate_rpc_server_op(&random, &empty, &weights);
            // Without pending promises, can only get SmallDelay
            assert!(matches!(op, RpcServerOp::SmallDelay));
        }
    }
}
