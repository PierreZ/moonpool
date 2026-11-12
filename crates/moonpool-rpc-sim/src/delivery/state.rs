//! State keys the delivery campaign's roles share, and the peers' greeting
//! records.

use std::sync::{Arc, Mutex};

use moonpool_sim::StateHandle;

use super::judge::Judged;
use crate::foundations::state::Ledger;
use crate::lock;

/// How many times the server booted.
pub const SERVER_BOOTS_KEY: &str = "rpc.delivery.server.boots";
/// Lost-reply jobs the server received (each asks for a cut).
pub const CUT_REQUESTS_KEY: &str = "rpc.delivery.cuts";
/// Cuts the fault script has put in place (the server replies into the
/// cut only once its request is counted here).
pub const CUTS_INSTALLED_KEY: &str = "rpc.delivery.cuts.installed";
/// Crash jobs the server received (each asks for a crash).
pub const CRASH_REQUESTS_KEY: &str = "rpc.delivery.crashes";
/// The workload's IP, published when it starts.
pub const WORKLOAD_IP_KEY: &str = "rpc.delivery.workload.ip";
/// The workload's runtime handle, published when it starts, so the fault
/// script can see when the workload noticed a cut.
pub const WORKLOAD_RPC_KEY: &str = "rpc.delivery.workload.rpc";
/// Set once the fault script stopped serving crash and cut requests.
pub const SCRIPT_DONE_KEY: &str = "rpc.delivery.script.done";
/// The scripted resolver the workload bootstraps through.
pub const RESOLVER_KEY: &str = "rpc.delivery.resolver";
/// The bootstrap name of the server.
pub const SERVER_NAME: &str = "server.rpc";
/// Board label of the workload's runtime.
pub const WORKLOAD_LABEL: &str = "workload";
/// Source of run-unique greeting ids (both peers draw from it).
pub const GREET_IDS_KEY: &str = "rpc.delivery.greet.ids";
const GREET_LEDGER_KEY: &str = "rpc.delivery.greet.ledger";
const GREET_CALLS_KEY: &str = "rpc.delivery.greet.calls";

/// The peers' receipt ledger: a peer's greeting handler records each
/// greeting id here before it replies. It is separate from the server's
/// ledger, so the two id spaces never mix.
#[must_use]
pub fn greet_ledger(state: &StateHandle) -> Ledger {
    if let Some(ledger) = state.get::<Ledger>(GREET_LEDGER_KEY) {
        return ledger;
    }
    let ledger = Ledger::default();
    state.publish(GREET_LEDGER_KEY, ledger.clone());
    ledger
}

/// Every greeting the peers sent, with what its outcome proves. The
/// workload judges them against [`greet_ledger`] at the end of the run.
#[derive(Clone, Default)]
pub(crate) struct GreetCalls {
    calls: Arc<Mutex<Vec<Judged>>>,
}

impl GreetCalls {
    /// The run's greeting records, created on first use.
    pub(crate) fn of(state: &StateHandle) -> Self {
        if let Some(calls) = state.get::<Self>(GREET_CALLS_KEY) {
            return calls;
        }
        let calls = Self::default();
        state.publish(GREET_CALLS_KEY, calls.clone());
        calls
    }

    /// Record one finished greeting.
    pub(crate) fn push(&self, call: Judged) {
        lock(&self.calls).push(call);
    }

    /// Every greeting recorded so far.
    pub(crate) fn all(&self) -> Vec<Judged> {
        lock(&self.calls).clone()
    }
}
