//! Simulation test infrastructure for moonpool messaging layer.
//!
//! Provides shared utilities for testing EndpointMap, NetTransport,
//! and NetNotifiedQueue under chaos conditions.

#![allow(dead_code)]

pub mod invariants;
pub mod operations;
pub mod workloads;

use serde::{Deserialize, Serialize};

/// Test message format for tracking delivery via NetTransport.
///
/// Uses JSON serialization since NetNotifiedQueue uses serde_json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestMessage {
    /// Monotonically increasing sequence ID per sender
    pub seq_id: u64,
    /// Identifier for the sender workload
    pub sender_id: String,
    /// Whether this was sent via reliable queue
    pub reliable: bool,
    /// Optional payload for size testing
    #[serde(default)]
    pub payload: Vec<u8>,
}

impl TestMessage {
    /// Create a new test message.
    pub fn new(seq_id: u64, sender_id: impl Into<String>, reliable: bool) -> Self {
        Self {
            seq_id,
            sender_id: sender_id.into(),
            reliable,
            payload: Vec::new(),
        }
    }

    /// Create a test message with payload of specified size.
    pub fn with_payload(
        seq_id: u64,
        sender_id: impl Into<String>,
        reliable: bool,
        payload_size: usize,
    ) -> Self {
        Self {
            seq_id,
            sender_id: sender_id.into(),
            reliable,
            payload: vec![0xAB; payload_size],
        }
    }

    /// Serialize to JSON bytes for sending via NetTransport.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("TestMessage serialization should not fail")
    }
}

// ============================================================================
// RPC Message Types (Phase 12B)
// ============================================================================

/// RPC request message for simulation testing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcTestRequest {
    /// Unique request identifier for correlation tracking
    pub request_id: u64,
    /// Identifier for the client sending the request
    pub sender_id: String,
    /// Optional payload for size testing
    #[serde(default)]
    pub payload: Vec<u8>,
}

impl RpcTestRequest {
    /// Create an RPC test request with payload.
    pub fn with_payload(
        request_id: u64,
        sender_id: impl Into<String>,
        payload_size: usize,
    ) -> Self {
        Self {
            request_id,
            sender_id: sender_id.into(),
            payload: vec![0xAB; payload_size],
        }
    }
}

/// RPC response message for simulation testing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcTestResponse {
    /// Request ID this response corresponds to
    pub request_id: u64,
    /// Whether the request was processed successfully
    pub success: bool,
}

impl RpcTestResponse {
    /// Create a successful response.
    pub fn success(request_id: u64) -> Self {
        Self {
            request_id,
            success: true,
        }
    }
}

// ============================================================================
// Multi-Method Calculator Types (Phase 12D - Step 17)
// ============================================================================

/// Interface and method identifiers for calculator simulation.
pub mod calculator {
    /// Calculator interface identifier.
    pub const CALC_INTERFACE: u64 = 0xCA1C_0000;
    /// Add method identifier.
    pub const METHOD_ADD: u64 = 0;
    /// Subtract method identifier.
    pub const METHOD_SUB: u64 = 1;
    /// Multiply method identifier.
    pub const METHOD_MUL: u64 = 2;
}

/// Add request for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcAddRequest {
    /// First operand.
    pub a: i64,
    /// Second operand.
    pub b: i64,
    /// Request identifier.
    pub request_id: u64,
}

/// Add response for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcAddResponse {
    /// Computation result.
    pub result: i64,
    /// Request identifier.
    pub request_id: u64,
}

/// Subtract request for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcSubRequest {
    /// First operand.
    pub a: i64,
    /// Second operand.
    pub b: i64,
    /// Request identifier.
    pub request_id: u64,
}

/// Subtract response for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcSubResponse {
    /// Computation result.
    pub result: i64,
    /// Request identifier.
    pub request_id: u64,
}

/// Multiply request for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcMulRequest {
    /// First operand.
    pub a: i64,
    /// Second operand.
    pub b: i64,
    /// Request identifier.
    pub request_id: u64,
}

/// Multiply response for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcMulResponse {
    /// Computation result.
    pub result: i64,
    /// Request identifier.
    pub request_id: u64,
}

#[cfg(test)]
mod serialization_tests {
    use super::*;

    #[test]
    fn test_message_serialization_roundtrip() {
        let msg = TestMessage::with_payload(42, "client_1", true, 100);
        let bytes = msg.to_bytes();
        let decoded: TestMessage = serde_json::from_slice(&bytes).expect("should deserialize");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_minimal() {
        let msg = TestMessage::new(0, "", false);
        let bytes = msg.to_bytes();
        let decoded: TestMessage = serde_json::from_slice(&bytes).expect("should deserialize");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_rpc_request_serialization() {
        let req = RpcTestRequest::with_payload(123, "client_1", 50);
        let bytes = serde_json::to_vec(&req).expect("should serialize");
        let decoded: RpcTestRequest = serde_json::from_slice(&bytes).expect("should deserialize");
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_rpc_response_serialization() {
        let resp = RpcTestResponse::success(456);
        let bytes = serde_json::to_vec(&resp).expect("should serialize");
        let decoded: RpcTestResponse = serde_json::from_slice(&bytes).expect("should deserialize");
        assert_eq!(resp, decoded);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moonpool_sim::{SimulationBuilder, SimulationReport};

    use super::operations::ClientOpWeights;
    use super::workloads::{
        LocalDeliveryConfig, LocalDeliveryWorkload, MultiMethodWorkload, MultiNodeClientConfig,
        MultiNodeClientWorkload, MultiNodeServerConfig, MultiNodeServerWorkload, RpcWorkload,
        RpcWorkloadConfig,
    };

    fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        local_runtime.block_on(async move { builder.run().await })
    }

    fn assert_simulation_success(report: &SimulationReport) {
        if !report.seeds_failing.is_empty() {
            panic!(
                "Simulation had {} failing seeds: {:?}",
                report.seeds_failing.len(),
                report.seeds_failing
            );
        }
        if !report.assertion_violations.is_empty() {
            panic!(
                "Assertion violations:\n{}",
                report
                    .assertion_violations
                    .iter()
                    .map(|v| format!("  - {}", v))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    /// Test local delivery without chaos - verifies basic functionality.
    #[test]
    fn test_local_delivery_basic() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                    num_operations: 50,
                    weights: ClientOpWeights::default(),
                    receive_timeout: Duration::from_secs(5),
                }))
                .set_iterations(5),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test local delivery with reliable-focused operations.
    #[test]
    fn test_local_delivery_reliable() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                    num_operations: 100,
                    weights: ClientOpWeights::reliable_focused(),
                    receive_timeout: Duration::from_secs(5),
                }))
                .set_iterations(10),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test local delivery with unreliable-focused operations.
    #[test]
    fn test_local_delivery_unreliable() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(LocalDeliveryWorkload::new(LocalDeliveryConfig {
                    num_operations: 100,
                    weights: ClientOpWeights::unreliable_focused(),
                    receive_timeout: Duration::from_secs(5),
                }))
                .set_iterations(10),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test RPC basic request-response without chaos.
    #[test]
    fn test_rpc_basic_request_response() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(RpcWorkload::new(RpcWorkloadConfig::happy_path()))
                .set_iterations(5),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test RPC with broken promises.
    #[test]
    fn test_rpc_broken_promise() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(RpcWorkload::new(RpcWorkloadConfig::broken_promise_focused()))
                .set_iterations(10),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test multi-node RPC with 1 client and 1 server.
    #[test]
    fn test_multi_node_rpc_1x1() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(MultiNodeServerWorkload::new(
                    MultiNodeServerConfig::happy_path(),
                ))
                .workload(MultiNodeClientWorkload::new(
                    MultiNodeClientConfig::happy_path(),
                ))
                .set_iterations(5),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test multi-node RPC with 2 clients and 1 server.
    #[test]
    fn test_multi_node_rpc_2x1() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(MultiNodeServerWorkload::new(
                    MultiNodeServerConfig::happy_path(),
                ))
                .workload(MultiNodeClientWorkload::new(
                    MultiNodeClientConfig::happy_path(),
                ))
                .workload(MultiNodeClientWorkload::new(
                    MultiNodeClientConfig::happy_path(),
                ))
                .set_iterations(5),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }

    /// Test multi-method interface basic routing.
    #[test]
    fn test_multi_method_basic() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let report = run_simulation(
            SimulationBuilder::new()
                .workload(MultiMethodWorkload::new(50))
                .set_iterations(5),
        );

        println!("{}", report);
        assert_simulation_success(&report);
    }
}
