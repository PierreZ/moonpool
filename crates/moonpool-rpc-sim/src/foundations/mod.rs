//! The `sim-rpc-foundations` campaign (#213).
//!
//! Two process groups and one surviving workload:
//!
//! - **server** ([`ServerProcess`]): an RPC runtime per boot serving echo,
//!   slow, crash-after-receipt and a rotating ephemeral endpoint;
//! - **relay** ([`RelayProcess`]): a second runtime that forwards to the
//!   server, so process-to-process calls use the same code;
//! - **workload** ([`FoundationsWorkload`]): a client-only runtime that
//!   survives server crashes and drives the operation mix.
//!
//! **Oracle.** The workload generates every request id. Server handlers
//! record each receipt in a [`Ledger`](state::Ledger) *before* replying, and
//! assert that the body is exactly what was sent. At the end the workload
//! judges each of its own outcomes against the ledger: at most one receipt
//! per id (no retransmission), exactly one for a reply, none for anything
//! the error said was never admitted. The oracle never reads the transport.
//!
//! **Faults.** Network chaos (swarm per seed, bit flips included, knob
//! extremes under `Chaos::BuggifyKnobs`), a
//! scripted crash of the server right after a handler received a request,
//! raw malformed sessions, forged references (wrong method, schema, codec),
//! destroyed endpoints, stale incarnations after same-address restarts, and
//! callers that give up before the reply.

mod faults;
pub mod messages;
mod relay;
mod server;
pub mod state;
mod workload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_rpc::{RpcConfig, RpcHandle};
use moonpool_sim::{Chaos, ChaosMode, SimContext, SimProviders, SimulationBuilder, TimeProvider};

pub use faults::CrashAfterReceiptInjector;
pub use relay::RelayProcess;
pub use server::ServerProcess;
pub use workload::{FoundationsWorkload, Op, WorkloadConfig};

/// The port every campaign runtime listens on.
pub const RPC_PORT: u16 = 4500;

/// The per-call deadline workloads and the relay use.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// The runtime configuration every campaign process uses.
#[must_use]
pub fn rpc_config() -> RpcConfig {
    RpcConfig {
        max_frame_bytes: 64 * 1024,
        connect_timeout: Duration::from_secs(1),
        handshake_timeout: Duration::from_secs(2),
        ..RpcConfig::default()
    }
}

/// Publish a runtime's counters to the board every 100 ms of sim time.
async fn report_stats(
    rpc: &RpcHandle<SimProviders>,
    board: &state::Board,
    label: &str,
    ctx: &SimContext,
) {
    loop {
        if let Some(stats) = rpc.stats() {
            board.report(label, stats);
        }
        if ctx.time().sleep(Duration::from_millis(100)).await.is_err() {
            return;
        }
    }
}

/// One finished run as the workload saw it: its semantic history and the
/// transport counters summed over every runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// One line per operation: id, operation, outcome class.
    pub history: Vec<String>,
    /// Checksum mismatches observed by any runtime.
    pub checksum_failures: u64,
    /// Protocol violations observed by any runtime.
    pub protocol_violations: u64,
}

/// Where finished runs are appended, for tests to inspect.
pub type Observations = Arc<Mutex<Vec<RunRecord>>>;

/// The campaign's builder: process groups, workload, faults and chaos,
/// without an iteration policy (callers choose one).
#[must_use]
pub fn campaign(config: WorkloadConfig, observations: &Observations) -> SimulationBuilder {
    let observations = Arc::clone(observations);
    SimulationBuilder::new()
        .processes(1, || Box::new(ServerProcess))
        .processes(1, || Box::new(RelayProcess))
        .workload_factory(move || {
            Box::new(FoundationsWorkload::new(
                config.clone(),
                Arc::clone(&observations),
            ))
        })
        .fault_factory(|| Box::new(CrashAfterReceiptInjector::default()))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::BuggifyKnobs])
        .chaos_duration(Duration::from_secs(8))
}
