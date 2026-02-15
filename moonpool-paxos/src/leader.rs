//! Leader (primary) service for Vertical Paxos II.
//!
//! The leader drives consensus by coordinating Phase 1 and Phase 2 with
//! acceptors. In Raft terms, the leader is like the **Raft leader** — it
//! receives client commands and replicates them to followers (acceptors).
//!
//! ## Leader Lifecycle (Paper Section 4.3, Figure 4)
//!
//! ```text
//! 1. Master issues new_ballot(bal) → leader receives new config
//! 2. Leader runs Phase 1 (VFindSafe) against PREVIOUS config's acceptors
//!    - Sends Phase1a to each acceptor
//!    - Collects Phase1b responses
//!    - Determines SafeValue for each slot
//! 3. If any SafeValue::Bound found → leader re-commits those values via Phase 2
//! 4. Leader calls master.report_complete(bal, prev_bal)
//! 5. Master activates → leader can now serve client requests
//! 6. Leader serves: client → assign slot → Phase 2 → respond
//! ```
//!
//! ## Key Differences from Raft Leader
//!
//! | Aspect | Raft | VP II |
//! |---|---|---|
//! | **Election** | Cluster-internal vote | Master assigns leader |
//! | **Log recovery** | Leader overwrites followers | Phase 1 learns from prev config |
//! | **Replication quorum** | Majority | ALL acceptors |
//! | **Client redirect** | Via cluster gossip | Via master's current_config |
//!
//! ## Write Quorum = ALL
//!
//! The most important difference: VP II requires **every** acceptor to
//! acknowledge a Phase 2a before a value is "chosen." This means:
//! - Higher write latency (must wait for slowest acceptor)
//! - But: any single acceptor has ALL chosen values (read quorum = 1)
//! - Reconfiguration is trivial: new leader IS a read quorum

use std::cell::RefCell;
use std::rc::Rc;

use moonpool_core::{Endpoint, NetworkAddress, TimeProvider, UID};
use moonpool_transport::{JsonCodec, NetTransport, Providers, RequestStream, send_request};
use tracing::{debug, info, warn};

use crate::acceptor::{ACCEPTOR_INTERFACE_ID, PHASE1A_METHOD, PHASE2A_METHOD};
use crate::master::ConfigurationMaster;
use crate::storage::PaxosStorage;
use crate::types::{
    BallotNumber, CommandRequest, CommandResponse, Configuration, LogEntry, LogSlot, PaxosError,
    Phase1aRequest, Phase1bResponse, Phase2aRequest, Phase2bResponse, SafeValue,
};

// =============================================================================
// Leader RPC service
// =============================================================================

/// Interface ID for the Leader service (client-facing).
pub const LEADER_INTERFACE_ID: u64 = 0x1EAD_0001;

/// Method index for the submit command RPC.
pub const SUBMIT_METHOD: u64 = 1;

/// The leader's server-side RPC endpoint for client commands.
pub struct LeaderServer<C: moonpool_core::MessageCodec> {
    /// Request stream for client command submissions.
    pub submit: RequestStream<CommandRequest, C>,
}

impl LeaderServer<JsonCodec> {
    /// Register the leader's RPC endpoints on the given transport.
    pub fn init<P: Providers>(transport: &Rc<NetTransport<P>>) -> Self {
        let (submit_stream, _) =
            transport.register_handler_at(LEADER_INTERFACE_ID, SUBMIT_METHOD, JsonCodec);
        Self {
            submit: submit_stream,
        }
    }
}

// =============================================================================
// Leader State
// =============================================================================

/// The leader's mutable state.
///
/// Tracks the current configuration, ballot, next slot to assign, and
/// whether the leader has been activated by the master.
///
/// ## Raft Comparison
///
/// | Leader Field | Raft Equivalent | Purpose |
/// |---|---|---|
/// | `ballot` | `currentTerm` | The leader's current term/ballot |
/// | `config` | cluster membership | Who the acceptors are |
/// | `next_slot` | `nextIndex` (sort of) | Next log position to use |
/// | `activated` | leader state | Whether the leader can serve |
/// | `storage` | `log[]` | The leader's copy of the log |
pub struct LeaderState<S: PaxosStorage> {
    /// The current configuration this leader is operating in.
    config: Configuration,

    /// Whether this leader has been activated by the master.
    ///
    /// Before activation, the leader cannot serve client requests.
    /// Activation happens after Phase 1 is complete and the master confirms.
    activated: bool,

    /// The next log slot to assign to a client command.
    next_slot: LogSlot,

    /// Persistent storage for the leader's log.
    storage: S,
}

impl<S: PaxosStorage> LeaderState<S> {
    /// Create a new leader state for the given configuration.
    ///
    /// The leader starts **inactive** — it must complete Phase 1 and
    /// receive activation from the master before serving clients.
    pub fn new(config: Configuration, storage: S) -> Result<Self, PaxosError> {
        // Determine next_slot from existing storage
        let next_slot = match storage.highest_slot()? {
            Some(slot) => slot.next(),
            None => LogSlot::FIRST,
        };

        Ok(Self {
            config,
            activated: false,
            next_slot,
            storage,
        })
    }

    /// Get the leader's ballot number.
    pub fn ballot(&self) -> BallotNumber {
        self.config.ballot
    }

    /// Get the leader's configuration.
    pub fn config(&self) -> &Configuration {
        &self.config
    }

    /// Check if the leader has been activated.
    pub fn is_activated(&self) -> bool {
        self.activated
    }

    /// Mark the leader as activated.
    ///
    /// Called after the master confirms activation.
    pub fn activate(&mut self) {
        self.activated = true;
        info!(ballot = %self.config.ballot, "leader activated");
    }

    /// Get the next slot and advance the counter.
    fn allocate_slot(&mut self) -> LogSlot {
        let slot = self.next_slot;
        self.next_slot = slot.next();
        slot
    }

    /// Update next_slot if the given slot is >= current next_slot.
    ///
    /// Used after Phase 1 when we discover existing entries.
    fn advance_next_slot_past(&mut self, slot: LogSlot) {
        if slot >= self.next_slot {
            self.next_slot = slot.next();
        }
    }
}

// =============================================================================
// Phase 1: VFindSafe — Learning prior state
// =============================================================================

/// Execute Phase 1 (VFindSafe) for the given slot range.
///
/// Phase 1 is how a new leader discovers any values that may have been
/// chosen (or partially chosen) in the previous configuration. It sends
/// Phase1a messages to the **previous configuration's** acceptors and
/// collects their responses.
///
/// **Raft analogy**: This is like a new Raft leader reading uncommitted
/// entries from followers after winning an election. But VP II formalizes
/// this as a distinct protocol phase with explicit safety guarantees.
///
/// ## VFindSafe Algorithm (Paper Section 4.3)
///
/// For each slot:
/// 1. Send Phase1a(ballot, slot) to all previous-config acceptors
/// 2. Collect Phase1b responses (need at least one — read quorum = 1)
/// 3. If any acceptor voted: take the value from the highest-ballot vote
///    → SafeValue::Bound(value) — MUST re-propose this value
/// 4. If no acceptor voted: SafeValue::Free — can propose anything
///
/// Since write quorum = ALL in VP II, every acceptor has seen every chosen
/// value. So reading from ANY one acceptor (read quorum = 1) is sufficient
/// to discover all chosen values.
///
/// # Arguments
///
/// * `transport` - Network transport for sending RPC messages
/// * `ballot` - The new leader's ballot number
/// * `prev_acceptors` - Acceptors from the previous configuration
/// * `from_slot` - Start of the slot range to check
/// * `to_slot` - End of the slot range to check (inclusive)
///
/// # Returns
///
/// A vector of `(LogSlot, SafeValue)` pairs for each slot that had a
/// prior vote. Slots with no votes are implicitly `SafeValue::Free`.
pub async fn run_phase1<P: Providers>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    ballot: BallotNumber,
    prev_acceptors: &[NetworkAddress],
    from_slot: LogSlot,
    to_slot: LogSlot,
) -> Result<Vec<(LogSlot, SafeValue)>, PaxosError> {
    let mut results = Vec::new();

    // For each slot in the range, send Phase1a to all previous acceptors
    // and find the highest-ballot vote.
    //
    // Since read quorum = 1 in VP II (because write quorum = ALL), we
    // only need ONE response per slot. But we send to all for robustness
    // (in case some acceptors are down).
    let mut slot = from_slot;
    while slot <= to_slot {
        let safe_value = run_phase1_for_slot(transport, time, ballot, prev_acceptors, slot).await?;
        if let SafeValue::Bound(_) = &safe_value {
            results.push((slot, safe_value));
        }
        slot = slot.next();
    }

    Ok(results)
}

/// Run Phase 1 for a single slot.
///
/// Sends Phase1a to all previous-config acceptors and determines the
/// safe value for this slot.
async fn run_phase1_for_slot<P: Providers>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    ballot: BallotNumber,
    prev_acceptors: &[NetworkAddress],
    slot: LogSlot,
) -> Result<SafeValue, PaxosError> {
    let request = Phase1aRequest { ballot, slot };

    // Send Phase1a to all previous acceptors and collect responses.
    // We need at least one response (read quorum = 1).
    let mut highest_voted_ballot: Option<BallotNumber> = None;
    let mut highest_voted_value: Option<Vec<u8>> = None;
    let mut got_response = false;

    for acceptor_addr in prev_acceptors {
        let endpoint = make_acceptor_phase1a_endpoint(acceptor_addr);

        match send_phase1a(transport, time, &endpoint, request.clone()).await {
            Ok(response) => {
                got_response = true;
                // If this acceptor voted, check if it has the highest ballot
                if let Some(voted_bal) = response.voted_ballot
                    && (highest_voted_ballot.is_none()
                        || voted_bal > highest_voted_ballot.expect("checked above"))
                {
                    highest_voted_ballot = Some(voted_bal);
                    highest_voted_value = response.voted_value;
                }
            }
            Err(e) => {
                // Log but continue — we only need one response (read quorum = 1)
                warn!(
                    acceptor = %acceptor_addr,
                    slot = %slot,
                    error = %e,
                    "Phase1a failed for acceptor, trying others"
                );
            }
        }
    }

    if !got_response {
        return Err(PaxosError::QuorumNotReached { got: 0, needed: 1 });
    }

    match highest_voted_value {
        Some(value) => {
            debug!(
                slot = %slot,
                voted_ballot = %highest_voted_ballot.expect("value implies ballot"),
                "Phase1 found bound value"
            );
            Ok(SafeValue::Bound(value))
        }
        None => {
            debug!(slot = %slot, "Phase1 found slot is free");
            Ok(SafeValue::Free)
        }
    }
}

/// Send a Phase1a RPC to a single acceptor with timeout.
async fn send_phase1a<P: Providers>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    endpoint: &Endpoint,
    request: Phase1aRequest,
) -> Result<Phase1bResponse, PaxosError> {
    let future = send_request(transport, endpoint, request, JsonCodec)
        .map_err(|e| PaxosError::Network(e.to_string()))?;

    let timeout_duration = std::time::Duration::from_secs(5);
    match time.timeout(timeout_duration, future).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(reply_err)) => Err(PaxosError::Network(reply_err.to_string())),
        Err(_) => Err(PaxosError::Timeout),
    }
}

// =============================================================================
// Phase 2: Replication to ALL acceptors
// =============================================================================

/// Execute Phase 2 for a single slot: replicate a value to ALL acceptors.
///
/// Phase 2 is the "commit" phase. The leader sends the value to every
/// acceptor and waits for ALL of them to acknowledge. Only then is the
/// value considered "chosen."
///
/// **Raft analogy**: Like `AppendEntries` replication, but with two key
/// differences:
/// 1. VP II requires ALL acceptors to respond (not just majority)
/// 2. The leader waits for ALL responses before considering the value committed
///
/// This stronger requirement means higher write latency but enables the
/// critical VP II property: read quorum = 1 (any single acceptor has all
/// chosen values).
///
/// # Arguments
///
/// * `transport` - Network transport for sending RPC messages
/// * `ballot` - The leader's current ballot
/// * `acceptors` - All acceptors in the current configuration
/// * `slot` - The log slot for this value
/// * `value` - The value to replicate
///
/// # Returns
///
/// `Ok(())` if ALL acceptors accepted, `Err` otherwise.
pub async fn run_phase2<P: Providers>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    ballot: BallotNumber,
    acceptors: &[NetworkAddress],
    slot: LogSlot,
    value: Vec<u8>,
) -> Result<(), PaxosError> {
    let request = Phase2aRequest {
        ballot,
        slot,
        value,
    };

    let mut accepted_count = 0;
    let total_needed = acceptors.len();

    // Send Phase2a to ALL acceptors and collect responses.
    // ALL must accept for the value to be chosen (write quorum = all).
    for acceptor_addr in acceptors {
        let endpoint = make_acceptor_phase2a_endpoint(acceptor_addr);

        let response = send_phase2a(transport, time, &endpoint, request.clone()).await?;

        if response.accepted {
            accepted_count += 1;
        } else {
            // An acceptor rejected — it has moved to a higher ballot.
            // This means our ballot is stale. In Raft terms: we're a
            // leader with a stale term.
            return Err(PaxosError::StaleBallot {
                current: ballot,
                seen: response.ballot,
            });
        }
    }

    if accepted_count < total_needed {
        return Err(PaxosError::QuorumNotReached {
            got: accepted_count,
            needed: total_needed,
        });
    }

    debug!(
        ballot = %ballot,
        slot = %slot,
        acceptors = total_needed,
        "Phase2 complete: value chosen"
    );

    Ok(())
}

/// Send a Phase2a RPC to a single acceptor with timeout.
async fn send_phase2a<P: Providers>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    endpoint: &Endpoint,
    request: Phase2aRequest,
) -> Result<Phase2bResponse, PaxosError> {
    let future = send_request(transport, endpoint, request, JsonCodec)
        .map_err(|e| PaxosError::Network(e.to_string()))?;

    let timeout_duration = std::time::Duration::from_secs(5);
    match time.timeout(timeout_duration, future).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(reply_err)) => Err(PaxosError::Network(reply_err.to_string())),
        Err(_) => Err(PaxosError::Timeout),
    }
}

// =============================================================================
// Client Request Path
// =============================================================================

/// Handle a client command submission.
///
/// This is the main entry point for client requests. The leader:
/// 1. Checks it's activated (can serve)
/// 2. Assigns the next log slot
/// 3. Runs Phase 2 (replicate to all acceptors)
/// 4. Stores the entry in its own log
/// 5. Returns the slot number to the client
///
/// **Processing is sequential**: one command at a time. The leader does not
/// pipeline commands (this is a simplification for correctness; a production
/// implementation could batch).
///
/// **SafeValue constraint**: If Phase 1 discovered bound values, the leader
/// must re-commit those first before accepting new client commands. This is
/// enforced by the activation flow — the leader re-commits bound values
/// during Phase 1, before the master activates it.
pub async fn handle_client_command<P: Providers, S: PaxosStorage>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    state: &Rc<RefCell<LeaderState<S>>>,
    command: Vec<u8>,
) -> Result<CommandResponse, PaxosError> {
    let (ballot, acceptors, slot) = {
        let mut s = state.borrow_mut();

        if !s.is_activated() {
            return Err(PaxosError::NotActivated);
        }

        let ballot = s.ballot();
        let acceptors = s.config.acceptors.clone();
        let slot = s.allocate_slot();

        (ballot, acceptors, slot)
    };

    // Phase 2: replicate to all acceptors
    run_phase2(transport, time, ballot, &acceptors, slot, command.clone()).await?;

    // Store in leader's own log
    {
        let mut s = state.borrow_mut();
        s.storage.store_vote(LogEntry {
            slot,
            ballot,
            value: command,
        })?;
    }

    info!(ballot = %ballot, slot = %slot, "command committed");

    Ok(CommandResponse {
        slot,
        success: true,
    })
}

/// Run the full leader activation flow.
///
/// This implements the VP II activation protocol (Figure 5):
///
/// 1. Run Phase 1 (VFindSafe) against previous config's acceptors
/// 2. Re-commit any bound values found in Phase 1
/// 3. Report completion to the master
/// 4. Wait for master activation
///
/// After this function returns successfully, the leader is activated
/// and can begin serving client requests.
///
/// # Arguments
///
/// * `transport` - Network transport
/// * `time` - Time provider for timeouts
/// * `state` - The leader's state
/// * `master` - The configuration master
/// * `prev_config` - The previous configuration (whose acceptors to query in Phase 1)
pub async fn activate_leader<P: Providers, S: PaxosStorage>(
    transport: &Rc<NetTransport<P>>,
    time: &P::Time,
    state: &Rc<RefCell<LeaderState<S>>>,
    master: &Rc<RefCell<dyn ConfigurationMaster>>,
    prev_config: Option<&Configuration>,
) -> Result<(), PaxosError> {
    let ballot = state.borrow().ballot();

    // Step 1: Phase 1 (VFindSafe) — learn prior state from previous config
    if let Some(prev_cfg) = prev_config {
        // Determine slot range to check.
        // We check from FIRST to the highest slot the prev config might have used.
        // In practice, we'd query the prev acceptors for their highest slot.
        // For now, we check the range based on our storage.
        let from_slot = LogSlot::FIRST;
        let to_slot = {
            let s = state.borrow();
            match s.storage.highest_slot()? {
                Some(slot) => slot,
                None => LogSlot::FIRST,
            }
        };

        info!(
            ballot = %ballot,
            prev_ballot = %prev_cfg.ballot,
            from = %from_slot,
            to = %to_slot,
            "starting Phase 1 (VFindSafe)"
        );

        let bound_values = run_phase1(
            transport,
            time,
            ballot,
            &prev_cfg.acceptors,
            from_slot,
            to_slot,
        )
        .await?;

        // Step 2: Re-commit any bound values via Phase 2
        for (slot, safe_value) in &bound_values {
            if let SafeValue::Bound(value) = safe_value {
                let acceptors = state.borrow().config.acceptors.clone();
                run_phase2(transport, time, ballot, &acceptors, *slot, value.clone()).await?;

                // Update leader's storage and next_slot
                let mut s = state.borrow_mut();
                s.storage.store_vote(LogEntry {
                    slot: *slot,
                    ballot,
                    value: value.clone(),
                })?;
                s.advance_next_slot_past(*slot);

                debug!(
                    ballot = %ballot,
                    slot = %slot,
                    "re-committed bound value from Phase 1"
                );
            }
        }
    }

    // Step 3: Report completion to the master
    let prev_ballot = prev_config.map(|c| c.ballot).unwrap_or(BallotNumber::ZERO);

    {
        let mut m = master.borrow_mut();
        m.report_complete(ballot, prev_ballot)?;
    }

    // Step 4: Verify activation
    {
        let m = master.borrow();
        if !m.is_activated(ballot)? {
            return Err(PaxosError::NotActivated);
        }
    }

    // Mark leader as activated
    state.borrow_mut().activate();

    info!(ballot = %ballot, "leader activation complete");
    Ok(())
}

/// Run the leader event loop, serving client commands.
///
/// This function processes client command submissions sequentially.
/// It runs until the transport is closed or the leader loses its ballot.
pub async fn run_leader<P: Providers, S: PaxosStorage + 'static>(
    transport: Rc<NetTransport<P>>,
    time: P::Time,
    server: LeaderServer<JsonCodec>,
    state: Rc<RefCell<LeaderState<S>>>,
) {
    loop {
        let result = server
            .submit
            .recv_with_transport::<P, CommandResponse>(&transport)
            .await;

        match result {
            Some((request, reply)) => {
                match handle_client_command(&transport, &time, &state, request.command).await {
                    Ok(response) => {
                        reply.send(response);
                    }
                    Err(e) => {
                        warn!(error = %e, "client command failed");
                        reply.send(CommandResponse {
                            slot: LogSlot::FIRST,
                            success: false,
                        });
                    }
                }
            }
            None => break, // Transport closed
        }
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// Create an endpoint for sending Phase1a to an acceptor.
fn make_acceptor_phase1a_endpoint(addr: &NetworkAddress) -> Endpoint {
    let base = Endpoint::new(addr.clone(), UID::new(ACCEPTOR_INTERFACE_ID, 0));
    moonpool_transport::method_endpoint(&base, PHASE1A_METHOD as u32)
}

/// Create an endpoint for sending Phase2a to an acceptor.
fn make_acceptor_phase2a_endpoint(addr: &NetworkAddress) -> Endpoint {
    let base = Endpoint::new(addr.clone(), UID::new(ACCEPTOR_INTERFACE_ID, 0));
    moonpool_transport::method_endpoint(&base, PHASE2A_METHOD as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryPaxosStorage;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port)
    }

    fn make_config(ballot: u64, leader_port: u16) -> Configuration {
        Configuration {
            ballot: BallotNumber::new(ballot),
            leader: make_addr(leader_port),
            acceptors: vec![make_addr(5001), make_addr(5002), make_addr(5003)],
        }
    }

    #[test]
    fn test_leader_state_new() {
        let config = make_config(1, 5001);
        let storage = InMemoryPaxosStorage::new();
        let state = LeaderState::new(config.clone(), storage).expect("create");

        assert_eq!(state.ballot(), BallotNumber::new(1));
        assert!(!state.is_activated());
        assert_eq!(state.next_slot, LogSlot::FIRST);
    }

    #[test]
    fn test_leader_state_activate() {
        let config = make_config(1, 5001);
        let storage = InMemoryPaxosStorage::new();
        let mut state = LeaderState::new(config, storage).expect("create");

        assert!(!state.is_activated());
        state.activate();
        assert!(state.is_activated());
    }

    #[test]
    fn test_leader_allocate_slot() {
        let config = make_config(1, 5001);
        let storage = InMemoryPaxosStorage::new();
        let mut state = LeaderState::new(config, storage).expect("create");

        assert_eq!(state.allocate_slot(), LogSlot::new(0));
        assert_eq!(state.allocate_slot(), LogSlot::new(1));
        assert_eq!(state.allocate_slot(), LogSlot::new(2));
    }

    #[test]
    fn test_leader_advance_next_slot() {
        let config = make_config(1, 5001);
        let storage = InMemoryPaxosStorage::new();
        let mut state = LeaderState::new(config, storage).expect("create");

        // Advance past slot 5
        state.advance_next_slot_past(LogSlot::new(5));
        assert_eq!(state.next_slot, LogSlot::new(6));

        // Advance past slot 3 — should not go backwards
        state.advance_next_slot_past(LogSlot::new(3));
        assert_eq!(state.next_slot, LogSlot::new(6));
    }

    #[test]
    fn test_leader_state_with_existing_storage() {
        let mut storage = InMemoryPaxosStorage::new();
        storage
            .store_vote(LogEntry {
                slot: LogSlot::new(4),
                ballot: BallotNumber::new(1),
                value: b"existing".to_vec(),
            })
            .expect("store");

        let config = make_config(2, 5002);
        let state = LeaderState::new(config, storage).expect("create");

        // next_slot should be 5 (after the highest existing entry)
        assert_eq!(state.next_slot, LogSlot::new(5));
    }

    #[test]
    fn test_make_acceptor_endpoints() {
        let addr = make_addr(5001);

        let phase1a_ep = make_acceptor_phase1a_endpoint(&addr);
        let phase2a_ep = make_acceptor_phase2a_endpoint(&addr);

        // Both should point to the same address
        assert_eq!(phase1a_ep.address, addr);
        assert_eq!(phase2a_ep.address, addr);

        // But different tokens (different methods)
        assert_ne!(phase1a_ep.token, phase2a_ep.token);
    }
}
