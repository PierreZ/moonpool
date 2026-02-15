//! Acceptor service for Vertical Paxos II.
//!
//! The acceptor is the "voter" in the Paxos protocol. In Raft terms, acceptors
//! are similar to **followers** — they store log entries and vote on proposals
//! from the leader.
//!
//! ## Acceptor Responsibilities (Paper Section 4.1-4.2, Figure 3)
//!
//! An acceptor handles two types of messages:
//!
//! 1. **Phase 1a (Prepare)**: The leader asks "have you voted for this slot?"
//!    - If the message's ballot >= acceptor's maxBallot: update maxBallot, reply
//!      with any previous vote for this slot.
//!    - If the message's ballot < maxBallot: ignore (stale leader).
//!    - **Raft analogy**: Like receiving a `RequestVote` RPC — if the candidate's
//!      term is current, grant the vote.
//!
//! 2. **Phase 2a (Accept)**: The leader says "accept this value for this slot."
//!    - If the message's ballot >= maxBallot: store the vote, reply with 2b.
//!    - If the message's ballot < maxBallot: reject (stale leader).
//!    - **Raft analogy**: Like receiving an `AppendEntries` RPC — if the leader's
//!      term is current, append the entry to the log.
//!
//! ## Key Invariant
//!
//! An acceptor **never votes in a ballot lower than its maxBallot**. This is
//! the fundamental safety property. In Raft terms: a server never processes
//! AppendEntries from a leader with a stale term.
//!
//! ## State
//!
//! ```text
//! AcceptorState {
//!     maxBallot: BallotNumber,        // highest ballot promised (≈ currentTerm)
//!     votes: Map<LogSlot, LogEntry>,  // accepted values per slot (≈ log[])
//! }
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use moonpool_core::MessageCodec;
use moonpool_transport::rpc::ReplyPromise;
use moonpool_transport::{JsonCodec, NetTransport, Providers, RequestStream};
use tracing::{debug, warn};

use crate::storage::PaxosStorage;
use crate::types::{LogEntry, Phase1aRequest, Phase1bResponse, Phase2aRequest, Phase2bResponse};

// =============================================================================
// AcceptorService RPC definition
//
// We define the RPC service manually rather than using the #[service] macro
// because the acceptor handler needs mutable access to storage. The macro
// generates &self methods for RPC mode, but we need &mut self semantics
// for the acceptor's state. Instead, we use interior mutability (RefCell)
// with manual server/client setup.
// =============================================================================

/// Interface ID for the Acceptor service.
///
/// Registered in the transport layer for endpoint routing.
pub const ACCEPTOR_INTERFACE_ID: u64 = 0xACC0_0001;

/// Method index for the Phase 1a (prepare) RPC.
pub const PHASE1A_METHOD: u64 = 1;

/// Method index for the Phase 2a (accept) RPC.
pub const PHASE2A_METHOD: u64 = 2;

/// The acceptor's server-side RPC endpoint.
///
/// Holds request streams for Phase 1a and Phase 2a messages.
/// Created by calling [`AcceptorServer::init`] with a transport.
pub struct AcceptorServer<C: moonpool_core::MessageCodec> {
    /// Request stream for Phase 1a (prepare) messages.
    pub phase1a: RequestStream<Phase1aRequest, C>,
    /// Request stream for Phase 2a (accept) messages.
    pub phase2a: RequestStream<Phase2aRequest, C>,
}

impl AcceptorServer<JsonCodec> {
    /// Register the acceptor's RPC endpoints on the given transport.
    ///
    /// This sets up the request streams that will receive Phase 1a and
    /// Phase 2a messages from leaders.
    pub fn init<P: Providers>(transport: &Rc<NetTransport<P>>) -> Self {
        let (phase1a_stream, _) =
            transport.register_handler_at(ACCEPTOR_INTERFACE_ID, PHASE1A_METHOD, JsonCodec);
        let (phase2a_stream, _) =
            transport.register_handler_at(ACCEPTOR_INTERFACE_ID, PHASE2A_METHOD, JsonCodec);
        Self {
            phase1a: phase1a_stream,
            phase2a: phase2a_stream,
        }
    }
}

/// The acceptor's mutable state, wrapping a [`PaxosStorage`] backend.
///
/// This struct implements the Phase 1a and Phase 2a handlers per the
/// VP II paper (Section 4.1-4.2, Figure 3).
///
/// ## Raft Comparison
///
/// | Acceptor Field | Raft Equivalent | Purpose |
/// |---|---|---|
/// | `storage.max_ballot` | `currentTerm` | Highest term/ballot seen |
/// | `storage.votes[slot]` | `log[index]` | Accepted entries |
pub struct AcceptorState<S: PaxosStorage> {
    /// Persistent storage backend.
    storage: S,
}

impl<S: PaxosStorage> AcceptorState<S> {
    /// Create a new acceptor state with the given storage backend.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    /// Handle a Phase 1a (Prepare) message.
    ///
    /// **Protocol (VP II Figure 3, Phase 1a handler)**:
    ///
    /// ```text
    /// On receive Phase1a(bal, slot):
    ///   if bal >= maxBallot:
    ///     maxBallot = bal          // promise not to accept lower ballots
    ///     reply Phase1b(bal, slot, vote[slot].ballot, vote[slot].value)
    ///   else:
    ///     ignore                    // stale ballot, don't respond
    /// ```
    ///
    /// **Raft analogy**: This is like `RequestVote`:
    /// - The leader (candidate) sends its ballot (term) and asks about a slot.
    /// - If the ballot is current, the acceptor "grants the vote" by promising
    ///   not to accept lower ballots and returning any existing vote.
    /// - If the ballot is stale, the acceptor ignores the request (like Raft
    ///   rejecting a `RequestVote` from a candidate with a lower term).
    pub fn handle_phase1a(
        &mut self,
        request: Phase1aRequest,
    ) -> Result<Option<Phase1bResponse>, crate::types::PaxosError> {
        let max_ballot = self.storage.load_max_ballot()?;

        if request.ballot < max_ballot {
            // Stale ballot — ignore. In Raft terms: candidate's term < currentTerm.
            debug!(
                ballot = %request.ballot,
                max_ballot = %max_ballot,
                "ignoring Phase1a with stale ballot"
            );
            return Ok(None);
        }

        // Update maxBallot (promise not to accept lower ballots).
        // In Raft terms: updating currentTerm when we see a higher term.
        self.storage.store_max_ballot(request.ballot)?;

        // Look up any previous vote for this slot.
        // In Raft terms: checking if we already have a log entry at this index.
        let vote = self.storage.load_vote(request.slot)?;

        let response = Phase1bResponse {
            ballot: request.ballot,
            slot: request.slot,
            voted_ballot: vote.as_ref().map(|e| e.ballot),
            voted_value: vote.map(|e| e.value),
        };

        debug!(
            ballot = %request.ballot,
            slot = %request.slot,
            has_prior_vote = response.voted_ballot.is_some(),
            "handled Phase1a: returning promise"
        );

        Ok(Some(response))
    }

    /// Handle a Phase 2a (Accept) message.
    ///
    /// **Protocol (VP II Figure 3, Phase 2a handler)**:
    ///
    /// ```text
    /// On receive Phase2a(bal, slot, val):
    ///   if bal >= maxBallot:
    ///     maxBallot = bal
    ///     vote[slot] = (bal, val)    // store the vote
    ///     reply Phase2b(bal, slot, accepted=true)
    ///   else:
    ///     reply Phase2b(bal, slot, accepted=false)  // rejected
    /// ```
    ///
    /// **Raft analogy**: This is like `AppendEntries`:
    /// - The leader sends a value to replicate at a specific slot.
    /// - If the leader's ballot (term) is current, the acceptor accepts and
    ///   stores the value (like appending a log entry).
    /// - If the ballot is stale, the acceptor rejects (like Raft rejecting
    ///   AppendEntries from a leader with a lower term).
    ///
    /// **Key difference from Raft**: In VP II, the write quorum is ALL
    /// acceptors. The leader waits for every acceptor to respond before
    /// considering a value "chosen." In Raft, only a majority is needed.
    pub fn handle_phase2a(
        &mut self,
        request: Phase2aRequest,
    ) -> Result<Phase2bResponse, crate::types::PaxosError> {
        let max_ballot = self.storage.load_max_ballot()?;

        if request.ballot < max_ballot {
            // Stale ballot — reject. The leader should step down.
            warn!(
                ballot = %request.ballot,
                max_ballot = %max_ballot,
                slot = %request.slot,
                "rejecting Phase2a with stale ballot"
            );
            return Ok(Phase2bResponse {
                ballot: request.ballot,
                slot: request.slot,
                accepted: false,
            });
        }

        // Update maxBallot and store the vote.
        self.storage.store_max_ballot(request.ballot)?;
        self.storage.store_vote(LogEntry {
            slot: request.slot,
            ballot: request.ballot,
            value: request.value,
        })?;

        debug!(
            ballot = %request.ballot,
            slot = %request.slot,
            "accepted Phase2a: vote stored"
        );

        Ok(Phase2bResponse {
            ballot: request.ballot,
            slot: request.slot,
            accepted: true,
        })
    }
}

/// Run the acceptor event loop, processing Phase 1a and Phase 2a messages.
///
/// This function drives the acceptor by receiving messages from the transport
/// layer and dispatching them to the [`AcceptorState`] handlers. It runs
/// until the transport is closed.
///
/// # Arguments
///
/// * `transport` - The network transport for sending/receiving messages.
/// * `server` - The acceptor's registered RPC server.
/// * `state` - The acceptor's mutable state (wrapped in `Rc<RefCell<>>` for
///   shared ownership with the event loop).
pub async fn run_acceptor<P: Providers, S: PaxosStorage + 'static>(
    transport: Rc<NetTransport<P>>,
    server: AcceptorServer<JsonCodec>,
    state: Rc<RefCell<AcceptorState<S>>>,
) {
    loop {
        tokio::select! {
            result = server.phase1a.recv_with_transport::<P, Phase1bResponse>(&transport) => {
                match result {
                    Some((request, reply)) => {
                        handle_phase1a_rpc(&state, request, reply);
                    }
                    None => break, // Transport closed
                }
            }
            result = server.phase2a.recv_with_transport::<P, Phase2bResponse>(&transport) => {
                match result {
                    Some((request, reply)) => {
                        handle_phase2a_rpc(&state, request, reply);
                    }
                    None => break, // Transport closed
                }
            }
        }
    }
}

/// Handle a Phase 1a RPC request and send the response.
fn handle_phase1a_rpc<S: PaxosStorage, C: MessageCodec>(
    state: &Rc<RefCell<AcceptorState<S>>>,
    request: Phase1aRequest,
    reply: ReplyPromise<Phase1bResponse, C>,
) {
    let mut state = state.borrow_mut();
    match state.handle_phase1a(request) {
        Ok(Some(response)) => {
            reply.send(response);
        }
        Ok(None) => {
            // Stale ballot — don't reply (leader will timeout).
            // This is intentional: in VP II, ignoring stale messages
            // is the correct behavior.
        }
        Err(e) => {
            warn!(error = %e, "Phase1a handler error");
            // Don't reply — storage error means we can't safely respond.
        }
    }
}

/// Handle a Phase 2a RPC request and send the response.
fn handle_phase2a_rpc<S: PaxosStorage, C: MessageCodec>(
    state: &Rc<RefCell<AcceptorState<S>>>,
    request: Phase2aRequest,
    reply: ReplyPromise<Phase2bResponse, C>,
) {
    let mut state = state.borrow_mut();
    match state.handle_phase2a(request) {
        Ok(response) => {
            reply.send(response);
        }
        Err(e) => {
            warn!(error = %e, "Phase2a handler error");
            // Don't reply — storage error.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryPaxosStorage;
    use crate::types::{BallotNumber, LogSlot};

    fn make_acceptor() -> AcceptorState<InMemoryPaxosStorage> {
        AcceptorState::new(InMemoryPaxosStorage::new())
    }

    // =========================================================================
    // Phase 1a (Prepare) tests
    // =========================================================================

    #[test]
    fn test_phase1a_first_prepare() {
        let mut acceptor = make_acceptor();

        let resp = acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(1),
                slot: LogSlot::new(0),
            })
            .expect("handle")
            .expect("should respond");

        assert_eq!(resp.ballot, BallotNumber::new(1));
        assert_eq!(resp.slot, LogSlot::new(0));
        assert!(resp.voted_ballot.is_none());
        assert!(resp.voted_value.is_none());
    }

    #[test]
    fn test_phase1a_with_prior_vote() {
        let mut acceptor = make_acceptor();

        // First, accept a value at slot 0 via Phase 2a
        acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(1),
                slot: LogSlot::new(0),
                value: b"hello".to_vec(),
            })
            .expect("accept");

        // Now Phase 1a from a higher ballot should return the prior vote
        let resp = acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(2),
                slot: LogSlot::new(0),
            })
            .expect("handle")
            .expect("should respond");

        assert_eq!(resp.voted_ballot, Some(BallotNumber::new(1)));
        assert_eq!(resp.voted_value, Some(b"hello".to_vec()));
    }

    #[test]
    fn test_phase1a_stale_ballot_ignored() {
        let mut acceptor = make_acceptor();

        // Set maxBallot to 5 via a prepare
        acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(5),
                slot: LogSlot::new(0),
            })
            .expect("handle");

        // Phase 1a with ballot 3 should be ignored (stale)
        let resp = acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(3),
                slot: LogSlot::new(0),
            })
            .expect("handle");

        assert!(resp.is_none(), "stale ballot should be ignored");
    }

    #[test]
    fn test_phase1a_equal_ballot_accepted() {
        let mut acceptor = make_acceptor();

        // First prepare at ballot 5
        acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(5),
                slot: LogSlot::new(0),
            })
            .expect("handle");

        // Same ballot again should still be accepted (>= check, not >)
        let resp = acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(5),
                slot: LogSlot::new(1),
            })
            .expect("handle");

        assert!(resp.is_some(), "equal ballot should be accepted");
    }

    // =========================================================================
    // Phase 2a (Accept) tests
    // =========================================================================

    #[test]
    fn test_phase2a_accept_value() {
        let mut acceptor = make_acceptor();

        let resp = acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(1),
                slot: LogSlot::new(0),
                value: b"command-1".to_vec(),
            })
            .expect("accept");

        assert!(resp.accepted);
        assert_eq!(resp.ballot, BallotNumber::new(1));
        assert_eq!(resp.slot, LogSlot::new(0));

        // Verify the vote was stored
        let vote = acceptor
            .storage
            .load_vote(LogSlot::new(0))
            .expect("load")
            .expect("should exist");
        assert_eq!(vote.value, b"command-1");
        assert_eq!(vote.ballot, BallotNumber::new(1));
    }

    #[test]
    fn test_phase2a_stale_ballot_rejected() {
        let mut acceptor = make_acceptor();

        // Set maxBallot to 5
        acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(5),
                slot: LogSlot::new(0),
            })
            .expect("handle");

        // Phase 2a with ballot 3 should be rejected
        let resp = acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(3),
                slot: LogSlot::new(0),
                value: b"stale".to_vec(),
            })
            .expect("accept");

        assert!(!resp.accepted, "stale ballot should be rejected");
    }

    #[test]
    fn test_phase2a_overwrites_vote() {
        let mut acceptor = make_acceptor();

        // Accept at ballot 1
        acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(1),
                slot: LogSlot::new(0),
                value: b"first".to_vec(),
            })
            .expect("accept");

        // Accept at ballot 2 (higher) — should overwrite
        let resp = acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(2),
                slot: LogSlot::new(0),
                value: b"second".to_vec(),
            })
            .expect("accept");

        assert!(resp.accepted);

        let vote = acceptor
            .storage
            .load_vote(LogSlot::new(0))
            .expect("load")
            .expect("should exist");
        assert_eq!(vote.value, b"second");
        assert_eq!(vote.ballot, BallotNumber::new(2));
    }

    #[test]
    fn test_phase2a_multiple_slots() {
        let mut acceptor = make_acceptor();

        for i in 0..5 {
            let resp = acceptor
                .handle_phase2a(Phase2aRequest {
                    ballot: BallotNumber::new(1),
                    slot: LogSlot::new(i),
                    value: format!("cmd-{}", i).into_bytes(),
                })
                .expect("accept");
            assert!(resp.accepted);
        }

        // Verify all slots have votes
        let highest = acceptor.storage.highest_slot().expect("highest");
        assert_eq!(highest, Some(LogSlot::new(4)));
    }

    // =========================================================================
    // Combined Phase 1a + Phase 2a flow tests
    // =========================================================================

    #[test]
    fn test_full_accept_then_prepare_flow() {
        let mut acceptor = make_acceptor();

        // Ballot 1: accept a value at slot 0
        acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(1),
                slot: LogSlot::new(0),
                value: b"original".to_vec(),
            })
            .expect("accept");

        // Ballot 2: new leader does Phase 1a, discovers the prior vote
        let resp = acceptor
            .handle_phase1a(Phase1aRequest {
                ballot: BallotNumber::new(2),
                slot: LogSlot::new(0),
            })
            .expect("handle")
            .expect("should respond");

        assert_eq!(resp.voted_ballot, Some(BallotNumber::new(1)));
        assert_eq!(resp.voted_value, Some(b"original".to_vec()));

        // Ballot 2: new leader re-commits the discovered value
        let resp = acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(2),
                slot: LogSlot::new(0),
                value: b"original".to_vec(), // must re-propose same value
            })
            .expect("accept");
        assert!(resp.accepted);

        // Ballot 2: new leader can now propose new values at slot 1
        let resp = acceptor
            .handle_phase2a(Phase2aRequest {
                ballot: BallotNumber::new(2),
                slot: LogSlot::new(1),
                value: b"new-command".to_vec(),
            })
            .expect("accept");
        assert!(resp.accepted);
    }
}
