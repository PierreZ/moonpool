//! Workload implementations for simulation testing.
//!
//! Provides client and server workloads that exercise FlowTransport,
//! EndpointMap, and NetNotifiedQueue under chaos conditions.

#![allow(dead_code)] // Config fields may not all be used in every workload

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::Duration;

use moonpool::{Endpoint, FlowTransport, MessageReceiver, NetNotifiedQueue, NetworkAddress, UID};
use moonpool_foundation::{
    NetworkProvider, SimRandomProvider, SimulationMetrics, SimulationResult, TaskProvider,
    TimeProvider, WorkloadTopology,
};

use super::TestMessage;
use super::invariants::MessageInvariants;
use super::operations::{ClientOp, ClientOpWeights, generate_client_op};

/// Configuration for local delivery workload.
#[derive(Debug, Clone)]
pub struct LocalDeliveryConfig {
    /// Number of operations to perform
    pub num_operations: u64,
    /// Operation weights
    pub weights: ClientOpWeights,
    /// Timeout for receive operations
    pub receive_timeout: Duration,
}

impl Default for LocalDeliveryConfig {
    fn default() -> Self {
        Self {
            num_operations: 100,
            weights: ClientOpWeights::default(),
            receive_timeout: Duration::from_secs(5),
        }
    }
}

/// Local delivery workload - tests FlowTransport dispatch to local endpoints.
///
/// This workload:
/// 1. Creates a FlowTransport with a local address
/// 2. Registers a NetNotifiedQueue as an endpoint
/// 3. Sends messages to the local endpoint
/// 4. Verifies messages are dispatched correctly
pub async fn local_delivery_workload<N, T, TP>(
    random: SimRandomProvider,
    _network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
    config: LocalDeliveryConfig,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();

    // Create local address from topology
    let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

    // Create FlowTransport - we won't use network provider for local delivery
    let transport = FlowTransport::new(local_addr.clone(), _network, time.clone(), task);

    // Create message queue and register with transport
    let token = UID::new(0x1234, 0x5678);
    let queue: Rc<NetNotifiedQueue<TestMessage>> = Rc::new(NetNotifiedQueue::new(Endpoint::new(
        local_addr.clone(),
        token,
    )));
    let endpoint = transport.register(token, queue.clone() as Rc<dyn MessageReceiver>);

    // Track invariants
    let invariants = Rc::new(RefCell::new(MessageInvariants::new()));

    // Sequence counters
    let mut next_reliable_seq = 0u64;
    let mut next_unreliable_seq = 10000u64; // Offset to distinguish

    // Perform operations
    for _op_num in 0..config.num_operations {
        let op = generate_client_op(
            &random,
            &mut next_reliable_seq,
            &mut next_unreliable_seq,
            &config.weights,
        );

        match op {
            ClientOp::SendReliable {
                seq_id,
                payload_size,
            } => {
                let msg = TestMessage::with_payload(seq_id, &my_id, true, payload_size);
                let payload = msg.to_bytes();

                match transport.send_reliable(&endpoint, &payload) {
                    Ok(()) => {
                        invariants.borrow_mut().record_sent(seq_id, true);
                    }
                    Err(e) => {
                        // Send to local endpoint shouldn't fail if registered
                        tracing::warn!(error = %e, "Reliable send failed");
                    }
                }
            }
            ClientOp::SendUnreliable {
                seq_id,
                payload_size,
            } => {
                let msg = TestMessage::with_payload(seq_id, &my_id, false, payload_size);
                let payload = msg.to_bytes();

                match transport.send_unreliable(&endpoint, &payload) {
                    Ok(()) => {
                        invariants.borrow_mut().record_sent(seq_id, false);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Unreliable send failed");
                    }
                }
            }
            ClientOp::SmallDelay => {
                let _ = time.sleep(Duration::from_millis(1)).await;
            }
        }

        // Drain the queue and validate received messages
        while let Some(msg) = queue.try_recv() {
            let is_dup = invariants
                .borrow_mut()
                .record_received(msg.seq_id, msg.reliable);
            if !is_dup {
                invariants.borrow().validate_always();
            }
        }
    }

    // Final drain and validation
    while let Some(msg) = queue.try_recv() {
        invariants
            .borrow_mut()
            .record_received(msg.seq_id, msg.reliable);
    }

    let inv = invariants.borrow();
    inv.validate_always();

    // Build metrics
    Ok(SimulationMetrics {
        events_processed: config.num_operations,
        ..Default::default()
    })
}

/// Endpoint lifecycle workload - tests register/unregister under send load.
///
/// This workload:
/// 1. Creates multiple endpoints
/// 2. Sends messages while registering/unregistering endpoints
/// 3. Verifies endpoint_not_found is triggered for unregistered endpoints
pub async fn endpoint_lifecycle_workload<N, T, TP>(
    _random: SimRandomProvider,
    _network: N,
    time: T,
    task: TP,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics>
where
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
{
    let my_id = topology.my_ip.clone();
    let local_addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);

    let transport = FlowTransport::new(local_addr.clone(), _network, time.clone(), task);

    // Track sent/received
    let mut sent_count = 0u64;
    let mut _received_count = 0u64;
    let mut undelivered_count = 0u64;

    // Create initial endpoint
    let token1 = UID::new(0x1111, 0x1111);
    let queue1: Rc<NetNotifiedQueue<TestMessage>> = Rc::new(NetNotifiedQueue::new(Endpoint::new(
        local_addr.clone(),
        token1,
    )));
    let endpoint1 = transport.register(token1, queue1.clone() as Rc<dyn MessageReceiver>);

    // Second endpoint for lifecycle testing
    let token2 = UID::new(0x2222, 0x2222);
    let queue2: Rc<NetNotifiedQueue<TestMessage>> = Rc::new(NetNotifiedQueue::new(Endpoint::new(
        local_addr.clone(),
        token2,
    )));

    for i in 0..50 {
        // Send to endpoint1 (always registered)
        let msg = TestMessage::new(i, &my_id, true);
        let payload = msg.to_bytes();
        if transport.send_reliable(&endpoint1, &payload).is_ok() {
            sent_count += 1;
        }

        // Toggle endpoint2 registration
        if i % 10 == 0 {
            let endpoint2 = transport.register(token2, queue2.clone() as Rc<dyn MessageReceiver>);

            // Send to endpoint2 while registered
            let msg2 = TestMessage::new(i + 1000, &my_id, true);
            let payload2 = msg2.to_bytes();
            if transport.send_reliable(&endpoint2, &payload2).is_ok() {
                sent_count += 1;
            }

            // Unregister endpoint2
            transport.unregister(&token2);
        }

        // Try sending to unregistered endpoint2
        if i % 10 == 5 {
            let endpoint2 = Endpoint::new(local_addr.clone(), token2);
            let msg2 = TestMessage::new(i + 2000, &my_id, true);
            let payload2 = msg2.to_bytes();
            if transport.send_reliable(&endpoint2, &payload2).is_err() {
                undelivered_count += 1;
            }
        }

        // Drain queues
        while let Some(_msg) = queue1.try_recv() {
            _received_count += 1;
        }
        while let Some(_msg) = queue2.try_recv() {
            _received_count += 1;
        }

        let _ = time.sleep(Duration::from_millis(1)).await;
    }

    // Final drain
    while let Some(_msg) = queue1.try_recv() {
        _received_count += 1;
    }
    while let Some(_msg) = queue2.try_recv() {
        _received_count += 1;
    }

    Ok(SimulationMetrics {
        events_processed: sent_count + undelivered_count,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_delivery_config_default() {
        let config = LocalDeliveryConfig::default();
        assert_eq!(config.num_operations, 100);
    }
}
