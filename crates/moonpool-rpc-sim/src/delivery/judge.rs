//! The rules that relate one call's outcome to its receipts in a ledger.
//! The workload's calls to the server and the peers' greetings obey the
//! same rules, so both are judged here.

use moonpool_rpc::{Execution, RpcError};
use moonpool_sim::assert_always;

/// What an outcome proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// A reply came back.
    Replied,
    /// The request never reached a handler.
    NotAdmitted,
    /// Zero or one execution (zero or more for a reliable call).
    Maybe,
    /// The request reached a handler, but no reply came back.
    Executed,
}

/// Classify an outcome by what it proves about execution.
pub(crate) fn classify<T>(outcome: &Result<T, RpcError>) -> Class {
    match outcome {
        Ok(_) => Class::Replied,
        Err(error) => match error.execution() {
            Execution::NotAdmitted => Class::NotAdmitted,
            Execution::Executed => Class::Executed,
            _ => Class::Maybe,
        },
    }
}

/// One finished call, ready to be judged against a ledger.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Judged {
    /// The request id in the ledger.
    pub id: u64,
    /// The call was a single attempt (else reliable delivery).
    pub single: bool,
    /// What the outcome proves.
    pub class: Class,
    /// Retained requests the caller's runtime queued again while this
    /// call was in flight. The caller makes one call at a time, so this is
    /// an upper bound of the copies this call sent after the first one.
    pub resends: u64,
}

/// Check the receipts of one call against its outcome. Returns the receipt
/// count.
pub(crate) fn judge(call: &Judged, receipts: u32) -> u32 {
    if call.single {
        // The independent external check that nothing retransmits a
        // single attempt, on any path (a connection replacement too).
        assert_always!(
            receipts <= 1,
            "a single attempt never reaches a handler twice"
        );
        if call.class == Class::Executed {
            assert_always!(receipts == 1, "an executed outcome executed exactly once");
        }
    } else {
        // Reliable delivery may execute more than once, but only once per
        // copy it sent: never more than the first copy plus the resends.
        assert_always!(
            u64::from(receipts) <= 1 + call.resends,
            "a reliable request executes at most once per copy sent",
            { "id" => call.id, "receipts" => receipts, "resends" => call.resends }
        );
    }
    match call.class {
        Class::Replied | Class::Executed => {
            assert_always!(receipts >= 1, "a call that produced an outcome executed");
        }
        Class::NotAdmitted => {
            assert_always!(
                receipts == 0,
                "a call reported not admitted never reached a handler"
            );
        }
        Class::Maybe => {}
    }
    receipts
}
