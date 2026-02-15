//! Workload implementations for Paxos simulation testing.
//!
//! Each workload corresponds to a node role in the Vertical Paxos II cluster:
//!
//! - [`AcceptorWorkload`]: Runs an acceptor server that handles Phase 1a/2a messages.
//!   In Raft terms: a follower that stores log entries and votes on proposals.
//!
//! - [`LeaderWorkload`]: Runs the leader activation flow (Phase 1 → Phase 2 for
//!   bound values → report_complete → serve clients). The leader is also an acceptor
//!   (it participates in write quorum = ALL). In Raft terms: the leader.
//!
//! - [`ClientWorkload`]: Discovers the primary from the master and submits commands.
//!   In Raft terms: a client that sends requests to the leader.
//!
//! ## Shared State
//!
//! The [`ConfigurationMaster`] is shared via `Rc<RefCell<>>` across all workloads
//! in the same simulation iteration. This models VP II's external master service.
//! In production, the master would be a separate highly-available service; in
//! simulation, we co-locate it for simplicity.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    SimContext, SimProviders, SimulationResult, TimeProvider, Workload, assert_sometimes,
};
use moonpool_transport::{NetTransportBuilder, NetworkAddress};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use moonpool_paxos::acceptor::{AcceptorServer, AcceptorState, run_acceptor};
use moonpool_paxos::leader::{LeaderServer, LeaderState, activate_leader, handle_client_command};
use moonpool_paxos::master::{ConfigurationMaster, InMemoryConfigMaster};
use moonpool_paxos::storage::InMemoryPaxosStorage;
use moonpool_paxos::types::{BallotNumber, Configuration, LogSlot};

/// Port used by all paxos nodes for RPC.
const PAXOS_PORT: u16 = 4500;

/// Number of commands the client tries to submit.
const DEFAULT_NUM_COMMANDS: u64 = 10;

/// Type alias for the committed values map to reduce complexity.
type CommittedValuesMap = Rc<RefCell<HashMap<u64, (u64, Vec<u8>)>>>;

// ============================================================================
// Shared Cluster State
// ============================================================================

/// Shared state accessible by all workloads in a simulation iteration.
///
/// This models the VP II configuration master as an in-memory shared service.
/// In production, this would be a separate HA service (e.g., ZooKeeper/etcd).
#[derive(Clone)]
pub struct SharedClusterState {
    /// The configuration master, shared across all workloads.
    pub master: Rc<RefCell<InMemoryConfigMaster>>,

    /// Tracks which values were committed at which slots, for invariant checking.
    /// Maps slot → (ballot, value). Published to simulation state for invariants.
    pub committed_values: CommittedValuesMap,
}

impl SharedClusterState {
    /// Create a new shared cluster state with initial configuration.
    pub fn new(initial_config: Configuration) -> Self {
        let mut master = InMemoryConfigMaster::new();
        master
            .bootstrap(initial_config)
            .expect("bootstrap should succeed for initial config");

        Self {
            master: Rc::new(RefCell::new(master)),
            committed_values: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

/// Snapshot of committed values for cross-workload invariant checking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommittedValuesSnapshot {
    /// Maps slot number → (ballot number, committed value).
    /// If two entries exist for the same slot with different values, safety is violated.
    pub entries: HashMap<u64, Vec<(u64, Vec<u8>)>>,

    /// Total commands committed.
    pub total_committed: u64,

    /// Whether the leader was activated at least once.
    pub leader_activated: bool,
}

// ============================================================================
// Acceptor Workload
// ============================================================================

/// Configuration for the acceptor workload.
#[derive(Debug, Clone)]
pub struct AcceptorConfig {
    /// Maximum time the acceptor runs before shutting down.
    pub run_duration: Duration,
}

impl Default for AcceptorConfig {
    fn default() -> Self {
        Self {
            run_duration: Duration::from_secs(30),
        }
    }
}

/// Acceptor workload — runs an acceptor server that handles Phase 1a/2a.
///
/// In Raft terms: this is a follower that stores log entries and responds
/// to the leader's AppendEntries and RequestVote RPCs.
///
/// In VP II: the acceptor stores votes (accepted values per slot) and
/// responds to Phase 1a (prepare) and Phase 2a (accept) messages.
pub struct AcceptorWorkload {
    config: AcceptorConfig,
    name: String,
}

impl AcceptorWorkload {
    /// Create a new acceptor workload with the given name.
    pub fn new(name: impl Into<String>, config: AcceptorConfig) -> Self {
        Self {
            config,
            name: name.into(),
        }
    }
}

#[async_trait(?Send)]
impl Workload for AcceptorWorkload {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        let local_addr = NetworkAddress::parse(&format!("{}:{}", my_id, PAXOS_PORT))
            .expect("valid acceptor address");

        info!(acceptor = %my_id, addr = %local_addr, "acceptor starting");

        // Build listening transport (accepts incoming connections from leader)
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr)
            .build_listening()
            .await
            .expect("acceptor should bind successfully");

        // Register Phase 1a and Phase 2a RPC handlers
        let server = AcceptorServer::init(&transport);

        // Create acceptor state with in-memory storage
        let storage = InMemoryPaxosStorage::new();
        let state = Rc::new(RefCell::new(AcceptorState::new(storage)));

        info!(acceptor = %my_id, "acceptor ready, processing messages");
        assert_sometimes!(true, "Acceptor should start successfully");

        // Run the acceptor event loop with a timeout.
        // The acceptor processes messages until the timeout or transport closes.
        let shutdown = ctx.shutdown().clone();
        tokio::select! {
            _ = run_acceptor(transport, server, state) => {
                debug!(acceptor = %my_id, "acceptor event loop ended");
            }
            _ = shutdown.cancelled() => {
                debug!(acceptor = %my_id, "acceptor shutting down via signal");
            }
            _ = ctx.time().sleep(self.config.run_duration) => {
                debug!(acceptor = %my_id, "acceptor run duration elapsed");
            }
        }

        info!(acceptor = %my_id, "acceptor stopped");
        Ok(())
    }
}

// ============================================================================
// Leader Workload
// ============================================================================

/// Configuration for the leader workload.
#[derive(Debug, Clone)]
pub struct LeaderConfig {
    /// Number of commands to serve after activation.
    pub num_commands_to_serve: u64,
    /// Maximum time to wait for activation.
    pub activation_timeout: Duration,
    /// Maximum time to serve commands.
    pub serve_duration: Duration,
}

impl Default for LeaderConfig {
    fn default() -> Self {
        Self {
            num_commands_to_serve: DEFAULT_NUM_COMMANDS,
            activation_timeout: Duration::from_secs(10),
            serve_duration: Duration::from_secs(20),
        }
    }
}

/// Leader workload — runs the full VP II leader lifecycle.
///
/// Lifecycle:
/// 1. Start as an acceptor (the leader is also an acceptor in VP II)
/// 2. Run Phase 1 (VFindSafe) against previous config's acceptors
/// 3. Re-commit any bound values discovered in Phase 1
/// 4. Report completion to the master → get activated
/// 5. Serve client commands via Phase 2 (replicate to ALL acceptors)
///
/// In Raft terms: this is like a leader that first catches up on uncommitted
/// entries, then starts serving client requests.
pub struct LeaderWorkload {
    config: LeaderConfig,
    shared: SharedClusterState,
}

impl LeaderWorkload {
    /// Create a new leader workload.
    pub fn new(config: LeaderConfig, shared: SharedClusterState) -> Self {
        Self { config, shared }
    }
}

#[async_trait(?Send)]
impl Workload for LeaderWorkload {
    fn name(&self) -> &str {
        "leader"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        let local_addr = NetworkAddress::parse(&format!("{}:{}", my_id, PAXOS_PORT))
            .expect("valid leader address");

        info!(leader = %my_id, addr = %local_addr, "leader starting");

        // Build listening transport (accepts connections from clients and other nodes)
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr.clone())
            .build_listening()
            .await
            .expect("leader should bind successfully");

        // Register acceptor RPC handlers (leader is also an acceptor)
        let _acceptor_server = AcceptorServer::init(&transport);

        // Register leader RPC handlers (client-facing submit endpoint)
        let leader_server = LeaderServer::init(&transport);

        // Get current configuration from master
        let config = {
            let m = self.shared.master.borrow();
            m.current_config()
                .expect("master should have config")
                .expect("config should exist")
        };

        info!(
            leader = %my_id,
            ballot = %config.ballot,
            acceptors = config.acceptors.len(),
            "leader has config, activating"
        );

        // Create leader state
        let storage = InMemoryPaxosStorage::new();
        let leader_state = Rc::new(RefCell::new(
            LeaderState::new(config.clone(), storage).expect("leader state creation"),
        ));

        // Small delay to let acceptors start up and bind their listeners
        let _ = ctx.time().sleep(Duration::from_millis(100)).await;

        // Activate the leader (Phase 1 → re-commit → report_complete)
        // For the initial bootstrap (no previous config), this is a no-op Phase 1.
        let master_ref: Rc<RefCell<dyn ConfigurationMaster>> = self.shared.master.clone();
        match activate_leader(&transport, ctx.time(), &leader_state, &master_ref, None).await {
            Ok(()) => {
                info!(leader = %my_id, "leader activated successfully");
                assert_sometimes!(true, "Leader should activate successfully");

                // Publish that leader was activated
                let snapshot = CommittedValuesSnapshot {
                    leader_activated: true,
                    ..Default::default()
                };
                ctx.state().publish("paxos_committed", snapshot);
            }
            Err(e) => {
                warn!(leader = %my_id, error = %e, "leader activation failed");
                return Ok(());
            }
        }

        // Serve client commands
        let mut commands_served = 0u64;
        let shutdown = ctx.shutdown().clone();
        let serve_deadline = ctx.time().sleep(self.config.serve_duration);

        tokio::pin!(serve_deadline);

        loop {
            tokio::select! {
                result = leader_server.submit.recv_with_transport::<SimProviders, moonpool_paxos::types::CommandResponse>(&transport) => {
                    match result {
                        Some((request, reply)) => {
                            debug!(leader = %my_id, "received client command");

                            match handle_client_command(
                                &transport,
                                ctx.time(),
                                &leader_state,
                                request.command.clone(),
                            ).await {
                                Ok(response) => {
                                    // Track committed value for invariant checking
                                    {
                                        let mut committed = self.shared.committed_values.borrow_mut();
                                        committed.insert(
                                            response.slot.0,
                                            (config.ballot.0, request.command.clone()),
                                        );
                                    }

                                    commands_served += 1;
                                    info!(
                                        leader = %my_id,
                                        slot = %response.slot,
                                        commands_served = commands_served,
                                        "command committed"
                                    );

                                    assert_sometimes!(true, "Leader should commit commands");

                                    // Publish updated committed values
                                    let snapshot = build_snapshot(&self.shared.committed_values, true);
                                    ctx.state().publish("paxos_committed", snapshot);

                                    reply.send(response);
                                }
                                Err(e) => {
                                    warn!(leader = %my_id, error = %e, "command failed");
                                    reply.send(moonpool_paxos::types::CommandResponse {
                                        slot: LogSlot::FIRST,
                                        success: false,
                                    });
                                }
                            }
                        }
                        None => {
                            debug!(leader = %my_id, "leader submit stream closed");
                            break;
                        }
                    }
                }
                _ = shutdown.cancelled() => {
                    debug!(leader = %my_id, "leader shutting down");
                    break;
                }
                _ = &mut serve_deadline => {
                    debug!(leader = %my_id, "leader serve duration elapsed");
                    break;
                }
            }
        }

        info!(
            leader = %my_id,
            commands_served = commands_served,
            "leader stopped"
        );

        Ok(())
    }
}

// ============================================================================
// Client Workload
// ============================================================================

/// Configuration for the client workload.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Number of commands to submit.
    pub num_commands: u64,
    /// Delay between command submissions.
    pub inter_command_delay: Duration,
    /// Timeout for each command submission.
    pub command_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            num_commands: DEFAULT_NUM_COMMANDS,
            inter_command_delay: Duration::from_millis(50),
            command_timeout: Duration::from_secs(10),
        }
    }
}

/// Client workload — discovers the primary and submits commands.
///
/// The client:
/// 1. Queries the master for the current leader
/// 2. Sends commands to the leader via RPC
/// 3. Tracks committed slots for invariant verification
///
/// In Raft terms: a client that sends requests to the leader, with
/// discovery going through the master (not the cluster).
pub struct ClientWorkload {
    config: ClientConfig,
    shared: SharedClusterState,
}

impl ClientWorkload {
    /// Create a new client workload.
    pub fn new(config: ClientConfig, shared: SharedClusterState) -> Self {
        Self { config, shared }
    }
}

#[async_trait(?Send)]
impl Workload for ClientWorkload {
    fn name(&self) -> &str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let my_id = ctx.my_ip().to_string();
        info!(client = %my_id, "client starting");

        let local_addr = NetworkAddress::parse(&format!("{}:{}", my_id, PAXOS_PORT + 1))
            .expect("valid client address");

        // Build transport (client doesn't need to listen)
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(local_addr)
            .build()
            .expect("client transport should build");

        // Wait for the leader to activate before submitting commands
        let _ = ctx.time().sleep(Duration::from_millis(500)).await;

        // Create a PaxosClient for command submission
        let master_ref: Rc<RefCell<dyn ConfigurationMaster>> = self.shared.master.clone();
        let mut client = moonpool_paxos::PaxosClient::new(master_ref);

        let mut commands_committed = 0u64;
        let mut commands_failed = 0u64;

        for i in 0..self.config.num_commands {
            let command = format!("cmd-{}", i).into_bytes();

            debug!(client = %my_id, command_idx = i, "submitting command");

            match client.submit(&transport, ctx.time(), command).await {
                Ok(response) => {
                    if response.success {
                        commands_committed += 1;
                        info!(
                            client = %my_id,
                            slot = %response.slot,
                            command_idx = i,
                            "command committed"
                        );
                        assert_sometimes!(true, "Client should get successful responses");
                    } else {
                        commands_failed += 1;
                        warn!(client = %my_id, command_idx = i, "command rejected");
                    }
                }
                Err(e) => {
                    commands_failed += 1;
                    warn!(
                        client = %my_id,
                        command_idx = i,
                        error = %e,
                        "command submission failed"
                    );
                    assert_sometimes!(true, "Client should sometimes see failures");
                }
            }

            // Small delay between commands
            if i < self.config.num_commands - 1 {
                let _ = ctx.time().sleep(self.config.inter_command_delay).await;
            }
        }

        info!(
            client = %my_id,
            committed = commands_committed,
            failed = commands_failed,
            total = self.config.num_commands,
            "client finished"
        );

        // At least some commands should commit in the happy path
        assert_sometimes!(
            commands_committed > 0,
            "Client should commit at least one command"
        );

        Ok(())
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Build a snapshot of committed values for invariant checking.
fn build_snapshot(
    committed: &CommittedValuesMap,
    leader_activated: bool,
) -> CommittedValuesSnapshot {
    let committed = committed.borrow();
    let mut entries: HashMap<u64, Vec<(u64, Vec<u8>)>> = HashMap::new();
    for (&slot, (ballot, value)) in committed.iter() {
        entries
            .entry(slot)
            .or_default()
            .push((*ballot, value.clone()));
    }

    CommittedValuesSnapshot {
        entries,
        total_committed: committed.len() as u64,
        leader_activated,
    }
}

/// Create the initial configuration for the cluster.
///
/// Uses the simulation topology to determine acceptor addresses.
/// The first acceptor is designated as the initial leader (this is
/// arbitrary — in VP II, the master chooses the leader).
pub fn make_initial_config(acceptor_ips: &[String]) -> Configuration {
    let acceptor_addrs: Vec<NetworkAddress> = acceptor_ips
        .iter()
        .map(|ip| {
            NetworkAddress::parse(&format!("{}:{}", ip, PAXOS_PORT))
                .expect("valid acceptor address")
        })
        .collect();

    Configuration {
        ballot: BallotNumber::new(1),
        leader: acceptor_addrs
            .first()
            .expect("at least one acceptor")
            .clone(),
        acceptors: acceptor_addrs,
    }
}
