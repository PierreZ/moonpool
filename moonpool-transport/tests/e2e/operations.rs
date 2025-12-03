//! Operation alphabet for Peer E2E testing.
//!
//! Defines all possible operations that can be performed on a Peer,
//! enabling randomized exploration of the state space.

use moonpool_transport::{
    NetworkProvider, Peer, PeerError, RandomProvider, TaskProvider, TimeProvider,
};

use super::{TestMessage, test_message_token};

/// Operations that can be performed on a Peer.
///
/// The simulation randomly selects from these operations to explore
/// different execution paths and timing combinations.
#[derive(Debug, Clone)]
pub enum PeerOp {
    /// Send a message via the reliable queue (will retry on failure).
    SendReliable {
        sequence_id: u64,
        payload_size: usize,
    },

    /// Send a message via the unreliable queue (dropped on failure).
    SendUnreliable {
        sequence_id: u64,
        payload_size: usize,
    },

    /// Attempt to receive a message (blocking).
    Receive,

    /// Attempt to receive a message without blocking.
    TryReceive,

    /// Force the peer to reconnect.
    ForceReconnect,

    /// Check and record peer metrics.
    CheckMetrics,

    /// Small delay to vary timing.
    SmallDelay,
}

/// Result of executing a peer operation.
#[derive(Debug)]
pub enum OpResult {
    /// Message was queued for sending.
    Sent { sequence_id: u64, reliable: bool },
    /// Message was received.
    Received { message: TestMessage },
    /// No message available (TryReceive returned None).
    NoMessage,
    /// Operation failed with error.
    Failed { error: String },
    /// Metrics were recorded.
    MetricsRecorded {
        queue_size: usize,
        is_connected: bool,
    },
    /// Delay completed.
    Delayed,
    /// Reconnect initiated.
    Reconnected,
}

/// Generate a random operation based on weights.
///
/// Weights control the probability distribution:
/// - Higher weight = more frequent selection
/// - Adjust weights to focus testing on specific behaviors
pub fn generate_operation<R: RandomProvider>(
    random: &R,
    next_reliable_seq: &mut u64,
    next_unreliable_seq: &mut u64,
    weights: &OpWeights,
) -> PeerOp {
    let total_weight = weights.send_reliable
        + weights.send_unreliable
        + weights.receive
        + weights.try_receive
        + weights.force_reconnect
        + weights.check_metrics
        + weights.small_delay;

    let choice = random.random_range(0..total_weight);

    let mut cumulative = 0;

    cumulative += weights.send_reliable;
    if choice < cumulative {
        let seq = *next_reliable_seq;
        *next_reliable_seq += 1;
        let payload_size = random.random_range(0..1000);
        return PeerOp::SendReliable {
            sequence_id: seq,
            payload_size,
        };
    }

    cumulative += weights.send_unreliable;
    if choice < cumulative {
        let seq = *next_unreliable_seq;
        *next_unreliable_seq += 1;
        let payload_size = random.random_range(0..1000);
        return PeerOp::SendUnreliable {
            sequence_id: seq,
            payload_size,
        };
    }

    cumulative += weights.receive;
    if choice < cumulative {
        return PeerOp::Receive;
    }

    cumulative += weights.try_receive;
    if choice < cumulative {
        return PeerOp::TryReceive;
    }

    cumulative += weights.force_reconnect;
    if choice < cumulative {
        return PeerOp::ForceReconnect;
    }

    cumulative += weights.check_metrics;
    if choice < cumulative {
        return PeerOp::CheckMetrics;
    }

    PeerOp::SmallDelay
}

/// Weights for operation selection.
///
/// Adjust these to focus testing on different behaviors.
#[derive(Debug, Clone)]
pub struct OpWeights {
    pub send_reliable: u32,
    pub send_unreliable: u32,
    pub receive: u32,
    pub try_receive: u32,
    pub force_reconnect: u32,
    pub check_metrics: u32,
    pub small_delay: u32,
}

impl Default for OpWeights {
    fn default() -> Self {
        Self {
            send_reliable: 40,   // Most common: sending reliably
            send_unreliable: 20, // Less common: unreliable sends
            receive: 25,         // Frequent: receiving messages
            try_receive: 5,      // Occasional: non-blocking receive
            force_reconnect: 3,  // Rare: force reconnection
            check_metrics: 5,    // Occasional: check metrics
            small_delay: 2,      // Rare: timing variation
        }
    }
}

impl OpWeights {
    /// Weights focused on reliable delivery testing.
    pub fn reliable_focused() -> Self {
        Self {
            send_reliable: 60,
            send_unreliable: 5,
            receive: 25,
            try_receive: 5,
            force_reconnect: 2,
            check_metrics: 2,
            small_delay: 1,
        }
    }

    /// Weights focused on unreliable delivery testing.
    pub fn unreliable_focused() -> Self {
        Self {
            send_reliable: 10,
            send_unreliable: 50,
            receive: 25,
            try_receive: 5,
            force_reconnect: 5,
            check_metrics: 3,
            small_delay: 2,
        }
    }

    /// Weights focused on reconnection testing.
    pub fn reconnection_focused() -> Self {
        Self {
            send_reliable: 30,
            send_unreliable: 10,
            receive: 20,
            try_receive: 5,
            force_reconnect: 25,
            check_metrics: 5,
            small_delay: 5,
        }
    }

    /// Weights for mixed queue priority testing.
    pub fn mixed_queues() -> Self {
        Self {
            send_reliable: 35,
            send_unreliable: 35,
            receive: 20,
            try_receive: 5,
            force_reconnect: 2,
            check_metrics: 2,
            small_delay: 1,
        }
    }
}

/// Execute a peer operation and return the result.
///
/// This function handles the actual execution of operations on the Peer,
/// recording results for invariant validation.
pub async fn execute_operation<N, T, TP>(
    peer: &mut Peer<N, T, TP>,
    op: PeerOp,
    sender_id: &str,
    time: &T,
) -> OpResult
where
    N: NetworkProvider + 'static,
    T: TimeProvider + 'static,
    TP: TaskProvider + 'static,
{
    match op {
        PeerOp::SendReliable {
            sequence_id,
            payload_size,
        } => {
            let msg = TestMessage::with_payload(sequence_id, sender_id, true, payload_size);
            let payload = msg.to_bytes();
            let token = test_message_token();

            match peer.send_reliable(token, &payload) {
                Ok(()) => OpResult::Sent {
                    sequence_id,
                    reliable: true,
                },
                Err(e) => OpResult::Failed {
                    error: format!("send_reliable failed: {:?}", e),
                },
            }
        }

        PeerOp::SendUnreliable {
            sequence_id,
            payload_size,
        } => {
            let msg = TestMessage::with_payload(sequence_id, sender_id, false, payload_size);
            let payload = msg.to_bytes();
            let token = test_message_token();

            match peer.send_unreliable(token, &payload) {
                Ok(()) => OpResult::Sent {
                    sequence_id,
                    reliable: false,
                },
                Err(e) => OpResult::Failed {
                    error: format!("send_unreliable failed: {:?}", e),
                },
            }
        }

        PeerOp::Receive => {
            // Use a timeout to avoid blocking forever
            match time
                .timeout(std::time::Duration::from_millis(100), peer.receive())
                .await
            {
                Ok(Ok(Ok((_token, payload)))) => match TestMessage::from_bytes(&payload) {
                    Ok(message) => OpResult::Received { message },
                    Err(e) => OpResult::Failed {
                        error: format!("deserialization failed: {:?}", e),
                    },
                },
                Ok(Ok(Err(PeerError::Disconnected))) => OpResult::Failed {
                    error: "peer disconnected".to_string(),
                },
                Ok(Ok(Err(e))) => OpResult::Failed {
                    error: format!("receive error: {:?}", e),
                },
                Ok(Err(())) | Err(_) => {
                    // Timeout - no message available
                    OpResult::NoMessage
                }
            }
        }

        PeerOp::TryReceive => match peer.try_receive() {
            Ok(Some((_token, payload))) => match TestMessage::from_bytes(&payload) {
                Ok(message) => OpResult::Received { message },
                Err(e) => OpResult::Failed {
                    error: format!("deserialization failed: {:?}", e),
                },
            },
            Ok(None) => OpResult::NoMessage,
            Err(e) => OpResult::Failed {
                error: format!("try_receive error: {:?}", e),
            },
        },

        PeerOp::ForceReconnect => {
            peer.reconnect();
            OpResult::Reconnected
        }

        PeerOp::CheckMetrics => {
            let metrics = peer.metrics();
            OpResult::MetricsRecorded {
                queue_size: peer.queue_size(),
                is_connected: metrics.is_connected,
            }
        }

        PeerOp::SmallDelay => {
            let _ = time.sleep(std::time::Duration::from_millis(1)).await;
            OpResult::Delayed
        }
    }
}
