//! The `sim-rpc-security` campaign (#218): who may call what, under key
//! and time transitions mixed with disconnects, crashes and graceful
//! shutdowns, across protocol versions.
//!
//! Roles:
//!
//! - **server** ([`SecurityServer`]): a verifying runtime (the production
//!   JWT/JWKS adapter over the shared rotating key set, the scripted UTC)
//!   serving a private and a public unary endpoint, a private stream and a
//!   private one-way endpoint, plus a local caller that sends its own
//!   requests through the same admission check. Crashed, rebooted and shut
//!   down gracefully (draining within a deadline) by the script.
//! - **legacy** ([`LegacyServer`]): a runtime pinned to protocol version 1
//!   with the default security, standing for an old build.
//! - **workload** ([`SecurityWorkload`]): a client runtime speaking both
//!   versions and one pinned to version 1; every request carries a drawn
//!   credential: none, valid, short-lived, not yet valid, from a retired
//!   key, forged, from an unknown key, for the wrong audience or issuer,
//!   symmetric (algorithm confusion) or garbage; reliable calls mint a
//!   fresh token for every retransmission.
//!
//! **Oracle.** An independent ledger ([`state::Ledger`]): callers record
//! each request and its credential before it leaves; handlers record each
//! receipt, with the principal the server verified, when user code gets
//! it, and assert on the spot that the credential could have been
//! accepted at some moment between sending and receipt (the script's UTC
//! only moves forward; key sets change by replacement): **no unauthorized
//! execution, ever**, on any route. Callers check that each refusal names
//! what is actually wrong with the credential and that a reply carries
//! its own request's principal; at the end every "not admitted" outcome
//! must have no receipt and every reply at least one. Nothing reads the
//! transport's state to decide what should have happened. Real
//! cryptographic interoperability (TLS, key types) is tested separately on
//! real TCP; this campaign's evidence is the trust policy's.
//!
//! **Faults.** Swarm network chaos (bit flips masked: [`crate::without_corruption`]) and a script ([`SecurityFaults`]) that
//! advances the UTC (steps and jumps), rotates keys, makes the server lose
//! the time, and crashes or gracefully reboots the server.

pub mod faults;
pub mod judge;
mod legacy;
pub mod messages;
mod server;
pub mod state;
pub mod trust;
mod workload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_sim::{Chaos, ChaosMode, SimulationBuilder};

pub use faults::SecurityFaults;
pub use legacy::LegacyServer;
pub use server::SecurityServer;
pub use workload::{SecurityConfig, SecurityOp, SecurityWorkload};

pub(crate) use crate::foundations::{CALL_TIMEOUT, RPC_PORT};
pub(crate) use crate::streams::pause;

/// One finished run as the workload saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityRecord {
    /// One line per request: id, operation, credential kind, outcome.
    pub history: Vec<String>,
}

/// Where finished runs are appended, for tests to inspect.
pub type SecurityRecords = Arc<Mutex<Vec<SecurityRecord>>>;

/// The campaign's builder without an iteration policy.
#[must_use]
pub fn security_campaign(config: SecurityConfig, records: &SecurityRecords) -> SimulationBuilder {
    let records = Arc::clone(records);
    SimulationBuilder::new()
        .processes(1, || Box::new(SecurityServer))
        .processes(1, || Box::new(LegacyServer))
        .workload_factory(move || {
            Box::new(SecurityWorkload::new(config.clone(), Arc::clone(&records)))
        })
        .fault_factory(|| Box::new(SecurityFaults::default()))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .network_fault_mask(crate::without_corruption())
        .chaos_duration(Duration::from_secs(8))
}
