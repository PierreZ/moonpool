//! Peer-level E2E workloads for simulation testing.
//!
//! Tests the Peer API directly (connection lifecycle, wire format,
//! reliable/unreliable delivery) under chaos conditions using the
//! Workload trait with setup/run/check lifecycle.

use std::cell::RefCell;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::runner::context::SimContext;
use moonpool_sim::runner::workload::Workload;
use moonpool_sim::{SimulationError, assert_always};
use moonpool_transport::peer::config::PeerConfig;
use moonpool_transport::peer::core::Peer;
use moonpool_transport::{NetworkProvider, Providers, TcpListenerTrait, TimeProvider};

use super::invariants::{PeerRefModel, check_always_invariants};
use super::operations::{OpResult, OpWeights, PeerOp, execute_operation, generate_operation};

/// Helper to parse address strings for Peer.
fn peer_address(ip: &str, port: u16) -> String {
    format!("{}:{}", ip, port)
}

// ============================================================================
// Server workload: Accept connection, receive and echo messages
// ============================================================================

/// Server workload for Peer-level E2E testing.
///
/// Binds a listener, accepts an incoming connection, creates an incoming Peer,
/// and processes messages until shutdown.
pub struct PeerServerWorkload {
    /// Port to listen on.
    port: u16,
}

impl PeerServerWorkload {
    /// Create a new server workload.
    pub fn new() -> Self {
        Self { port: 4500 }
    }
}

#[async_trait(?Send)]
impl Workload for PeerServerWorkload {
    fn name(&self) -> &str {
        "peer_server"
    }

    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        let addr = peer_address(ctx.my_ip(), self.port);
        let providers = ctx.providers().clone();

        // Bind listener
        let listener = providers
            .network()
            .bind(&addr)
            .await
            .map_err(|e| SimulationError::InvalidState(format!("bind failed: {}", e)))?;

        // Accept one connection
        let (stream, peer_addr) = listener
            .accept()
            .await
            .map_err(|e| SimulationError::InvalidState(format!("accept failed: {}", e)))?;

        // Create incoming peer
        let config = PeerConfig::local_network();
        let mut peer = Peer::new_incoming(providers.clone(), peer_addr, stream, config);

        // Process messages until shutdown
        while !ctx.shutdown().is_cancelled() {
            match ctx
                .time()
                .timeout(Duration::from_millis(50), peer.receive())
                .await
            {
                Ok(Ok((_token, _payload))) => {
                    // Message received successfully — server just consumes
                }
                Ok(Err(_)) => {
                    // Connection error — break out
                    break;
                }
                Err(_) => {
                    // Timeout — check shutdown and loop
                }
            }
        }

        Ok(())
    }
}

// ============================================================================
// Client workload: Connect to server, run alphabet operations
// ============================================================================

/// Client workload for Peer-level E2E testing.
///
/// Creates an outgoing Peer to the server, runs operations from the
/// PeerOp alphabet, and tracks results in a reference model.
pub struct PeerClientWorkload {
    /// Weights controlling operation distribution.
    weights: OpWeights,
    /// Number of operations to execute.
    ops_count: usize,
}

impl PeerClientWorkload {
    /// Create a new client workload with default settings.
    pub fn new() -> Self {
        Self {
            weights: OpWeights::default(),
            ops_count: 50,
        }
    }

    /// Create with specific weights and operation count.
    pub fn with_config(weights: OpWeights, ops_count: usize) -> Self {
        Self { weights, ops_count }
    }
}

#[async_trait(?Send)]
impl Workload for PeerClientWorkload {
    fn name(&self) -> &str {
        "peer_client"
    }

    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        let server_ip = &ctx.peers()[0];
        let server_addr = peer_address(server_ip, 4500);
        let providers = ctx.providers().clone();

        // Small delay to let server bind
        let _ = ctx.time().sleep(Duration::from_millis(10)).await;

        // Create outgoing peer
        let config = PeerConfig::local_network();
        let mut peer = Peer::new(providers.clone(), server_addr, config);

        // Wait for connection
        let _ = ctx.time().sleep(Duration::from_millis(50)).await;

        // Run operations
        let model = RefCell::new(PeerRefModel::new());
        let mut next_reliable_seq: u64 = 0;
        let mut next_unreliable_seq: u64 = 10_000;

        for i in 0..self.ops_count {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let op = generate_operation(
                ctx.random(),
                &mut next_reliable_seq,
                &mut next_unreliable_seq,
                &self.weights,
            );

            let result = execute_operation(&mut peer, op.clone(), self.name(), ctx.time()).await;

            // Track results in reference model
            {
                let mut m = model.borrow_mut();
                m.total_ops += 1;

                match (&op, &result) {
                    (
                        PeerOp::SendReliable {
                            sequence_id,
                            payload_size,
                        },
                        OpResult::Sent { .. },
                    ) => {
                        m.record_reliable_sent(*sequence_id, *payload_size);
                    }
                    (
                        PeerOp::SendUnreliable {
                            sequence_id,
                            payload_size,
                        },
                        OpResult::Sent { .. },
                    ) => {
                        m.record_unreliable_sent(*sequence_id, *payload_size);
                    }
                    (
                        PeerOp::SendReliable { .. } | PeerOp::SendUnreliable { .. },
                        OpResult::Failed { .. },
                    ) => {
                        m.record_send_failure();
                    }
                    (PeerOp::ForceReconnect, OpResult::Reconnected) => {
                        m.record_reconnect();
                    }
                    _ => {}
                }
            }

            // Periodic sleep to generate sim events and avoid deadlock detection
            if i % 5 == 4 {
                let _ = ctx.time().sleep(Duration::from_millis(1)).await;
            }
        }

        // Publish reference model for check phase
        ctx.state().publish("peer_client_model", model.into_inner());

        Ok(())
    }

    async fn check(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        if let Some(model) = ctx.state().get::<PeerRefModel>("peer_client_model") {
            check_always_invariants(&model);

            // Verify we actually ran operations
            assert_always!(
                model.total_ops > 0,
                "Client should have executed at least one operation"
            );
        }

        Ok(())
    }
}

// ============================================================================
// Local peer workload: Single-node peer testing
// ============================================================================

/// Local peer workload for single-node E2E testing.
///
/// Creates a listener and outgoing peer on the same node, exercising
/// the connection and delivery path locally. Useful for testing wire
/// format, queuing, and basic delivery without multi-node complexity.
pub struct LocalPeerWorkload {
    /// Weights controlling operation distribution.
    weights: OpWeights,
    /// Number of operations to execute.
    ops_count: usize,
}

impl LocalPeerWorkload {
    /// Create a new local peer workload with default settings.
    pub fn new() -> Self {
        Self {
            weights: OpWeights::reliable_focused(),
            ops_count: 30,
        }
    }

    /// Create with custom weights and operation count.
    pub fn with_config(weights: OpWeights, ops_count: usize) -> Self {
        Self { weights, ops_count }
    }
}

#[async_trait(?Send)]
impl Workload for LocalPeerWorkload {
    fn name(&self) -> &str {
        "local_peer"
    }

    async fn run(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        let addr = peer_address(ctx.my_ip(), 4500);
        let providers = ctx.providers().clone();

        // Bind listener
        let listener = providers
            .network()
            .bind(&addr)
            .await
            .map_err(|e| SimulationError::InvalidState(format!("bind failed: {}", e)))?;

        // Create outgoing peer to self
        let config = PeerConfig::local_network();
        let mut peer = Peer::new(providers.clone(), addr, config);

        // Accept the incoming connection (spawned as background task)
        let accept_providers = providers.clone();
        let time_for_accept = ctx.time().clone();
        let server_handle = tokio::task::spawn_local(async move {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let config = PeerConfig::local_network();
                    let mut incoming =
                        Peer::new_incoming(accept_providers, peer_addr, stream, config);

                    // Drain received messages until connection drops
                    loop {
                        match time_for_accept
                            .timeout(Duration::from_millis(200), incoming.receive())
                            .await
                        {
                            Ok(Ok(_)) => {} // consume message
                            Ok(Err(_)) => break,
                            Err(_) => break, // timeout — done
                        }
                    }
                }
                Err(_) => {} // accept failed
            }
        });

        // Wait for connection
        let _ = ctx.time().sleep(Duration::from_millis(20)).await;

        // Run operations
        let model = RefCell::new(PeerRefModel::new());
        let mut next_reliable_seq: u64 = 0;
        let mut next_unreliable_seq: u64 = 10_000;

        for i in 0..self.ops_count {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let op = generate_operation(
                ctx.random(),
                &mut next_reliable_seq,
                &mut next_unreliable_seq,
                &self.weights,
            );

            let result = execute_operation(&mut peer, op.clone(), self.name(), ctx.time()).await;

            // Track results
            {
                let mut m = model.borrow_mut();
                m.total_ops += 1;

                match (&op, &result) {
                    (
                        PeerOp::SendReliable {
                            sequence_id,
                            payload_size,
                        },
                        OpResult::Sent { .. },
                    ) => {
                        m.record_reliable_sent(*sequence_id, *payload_size);
                    }
                    (
                        PeerOp::SendUnreliable {
                            sequence_id,
                            payload_size,
                        },
                        OpResult::Sent { .. },
                    ) => {
                        m.record_unreliable_sent(*sequence_id, *payload_size);
                    }
                    (
                        PeerOp::SendReliable { .. } | PeerOp::SendUnreliable { .. },
                        OpResult::Failed { .. },
                    ) => {
                        m.record_send_failure();
                    }
                    (PeerOp::ForceReconnect, OpResult::Reconnected) => {
                        m.record_reconnect();
                    }
                    _ => {}
                }
            }

            // Periodic sleep to generate sim events
            if i % 3 == 2 {
                let _ = ctx.time().sleep(Duration::from_millis(1)).await;
            }
        }

        // Abort server task
        server_handle.abort();

        // Publish reference model
        ctx.state().publish("local_peer_model", model.into_inner());

        Ok(())
    }

    async fn check(&self, ctx: &SimContext) -> Result<(), SimulationError> {
        if let Some(model) = ctx.state().get::<PeerRefModel>("local_peer_model") {
            check_always_invariants(&model);

            assert_always!(
                model.total_ops > 0,
                "Local peer should have executed operations"
            );
        }

        Ok(())
    }
}

// ============================================================================
// Test scenarios
// ============================================================================

#[cfg(test)]
mod test_scenarios {
    use moonpool_sim::SimulationBuilder;

    use super::*;
    use crate::e2e_mod::operations::OpWeights;

    /// Helper: assert a report has no failed runs.
    fn assert_no_failures(report: &moonpool_sim::SimulationReport) {
        assert_eq!(
            report.failed_runs, 0,
            "Failed seeds: {:?}",
            report.seeds_failing
        );
    }

    #[test]
    fn test_local_peer_happy_path() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .expect("runtime");

        let report = runtime.block_on(async {
            SimulationBuilder::new()
                .workload(LocalPeerWorkload::new())
                .set_iterations(3)
                .run()
                .await
        });

        assert_no_failures(&report);
    }

    #[test]
    fn test_local_peer_reconnection_focused() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .expect("runtime");

        let report = runtime.block_on(async {
            SimulationBuilder::new()
                .workload(LocalPeerWorkload::with_config(
                    OpWeights::reconnection_focused(),
                    20,
                ))
                .set_iterations(3)
                .run()
                .await
        });

        assert_no_failures(&report);
    }

    #[test]
    fn test_peer_client_server() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .expect("runtime");

        let report = runtime.block_on(async {
            SimulationBuilder::new()
                .workload(PeerServerWorkload::new())
                .workload(PeerClientWorkload::with_config(
                    OpWeights::reliable_focused(),
                    30,
                ))
                .set_iterations(3)
                .run()
                .await
        });

        assert_no_failures(&report);
    }

    #[test]
    fn slow_simulation_peer_delivery() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(Default::default())
            .expect("runtime");

        let report = runtime.block_on(async {
            SimulationBuilder::new()
                .workload(LocalPeerWorkload::with_config(
                    OpWeights::mixed_queues(),
                    40,
                ))
                .set_iterations(50)
                .random_network()
                .run()
                .await
        });

        assert_no_failures(&report);
    }
}
