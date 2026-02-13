//! Simulation test infrastructure for moonpool messaging layer.
//!
//! Provides shared utilities for testing EndpointMap, NetTransport,
//! and NetNotifiedQueue under chaos conditions.

pub mod operations;

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
    pub const CALC_INTERFACE: u64 = 0xCA1C_0000;
    pub const METHOD_ADD: u64 = 0;
    pub const METHOD_SUB: u64 = 1;
    pub const METHOD_MUL: u64 = 2;
}

/// Add request for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcAddRequest {
    pub a: i64,
    pub b: i64,
    pub request_id: u64,
}

/// Add response for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcAddResponse {
    pub result: i64,
    pub request_id: u64,
}

/// Subtract request for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcSubRequest {
    pub a: i64,
    pub b: i64,
    pub request_id: u64,
}

/// Subtract response for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcSubResponse {
    pub result: i64,
    pub request_id: u64,
}

/// Multiply request for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcMulRequest {
    pub a: i64,
    pub b: i64,
    pub request_id: u64,
}

/// Multiply response for multi-method simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalcMulResponse {
    pub result: i64,
    pub request_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization_roundtrip() {
        let msg = TestMessage::with_payload(42, "client_1", true, 100);
        let bytes = msg.to_bytes();
        let decoded: TestMessage = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_message_minimal() {
        let msg = TestMessage::new(0, "", false);
        let bytes = msg.to_bytes();
        let decoded: TestMessage = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn test_rpc_request_serialization() {
        let req = RpcTestRequest::with_payload(123, "client_1", 50);
        let bytes = serde_json::to_vec(&req).unwrap();
        let decoded: RpcTestRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn test_rpc_response_serialization() {
        let resp = RpcTestResponse::success(456);
        let bytes = serde_json::to_vec(&resp).unwrap();
        let decoded: RpcTestResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(resp, decoded);
    }
}
