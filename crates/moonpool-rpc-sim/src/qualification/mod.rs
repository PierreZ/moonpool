//! The `sim-rpc-qualification` campaign (#219): every contract the other
//! campaigns tested apart, at once, under combined faults, with a declared
//! recovery phase and resource baselines.
//!
//! Roles:
//!
//! - **server** ([`QualServer`], three of them, in two datacenters by
//!   address): one verifying runtime per boot at the same `ip:port`. It
//!   serves a base `Work` instance (unary, one-way, reliable and balanced
//!   calls), recruits a fresh `Work` instance per configuration, produces
//!   the streams campaign's reply streams, answers the security
//!   campaign's credential-protected endpoint (JWT/JWKS over a rotating key
//!   set and a scripted UTC; a local caller goes through the same check),
//!   forwards calls to callbacks owned by a third process, and publishes
//!   everything through a well-known directory, possibly late after a boot.
//! - **third** ([`Third`]): adopts recruited members whose interfaces
//!   arrive inside a request and calls them through its own runtime; owns a
//!   callback endpoint servers call when a request hands them its
//!   reference.
//! - **legacy** ([`Legacy`]): a runtime pinned to protocol version 1 (the
//!   previous build in a rolling upgrade).
//! - **workload** ([`QualificationWorkload`]): a client runtime speaking
//!   versions 1 and 2, a version 1 only runtime and a load balancer. It
//!   learns interfaces only through directories and recruiters, keeps every
//!   publication it learned stored as bytes, and mixes at-most-once,
//!   reliable, sustained-failure-bounded and one-way calls, reply streams
//!   (eager, slow, abandoned before and after the first item), balanced
//!   calls (at most once, retried with permission, hedged, cancelled),
//!   credentialed calls (valid, expired, from a retired key, forged,
//!   anonymous), calls to stored interfaces of ended boots, recruitment
//!   through the third participant, callbacks, legacy and version 1 calls,
//!   overload bursts and a malformed raw peer, while a watcher probes
//!   servers that are draining for a graceful shutdown.
//!
//! **Oracles** (written by application code only, never read from the
//! transport): the qualification ledger ([`state::QualLedger`]: boots,
//! publications, executions), the streams campaign's producer/consumer
//! ledger, the security campaign's issue/receipt ledger and the workload's
//! own balanced-attempt ledger (fed by the observation hook). Handlers
//! assert on the spot that a request ran only in the boot and instance its
//! reference named and that no handler of an ended boot runs: a stale I1
//! never reaches I2, whatever carried it (a retained reliable call, a
//! stream, a balanced set, a forwarded or stored interface). At the end
//! every call is judged against the ledgers after late losers drained.
//!
//! **Recovery and baselines.** After the script stops, the workload must
//! regain every service (fresh publications, unary, a new stream, a
//! balanced call, a credentialed call, the legacy peer, recruitment and a
//! callback through the third participant) within [`RECOVERY_BOUND`];
//! then every stream a current boot produced must end within
//! [`RELEASE_BOUND`] and the client runtime and the queue model must be
//! back at their idle baseline. Every boot checks that the runtimes of its
//! process's earlier boots returned to their baseline
//! ([`ResourceProbe::is_at_baseline`](moonpool_rpc::ResourceProbe::is_at_baseline)),
//! and the end of the run checks them all.
//!
//! **Faults.** Swarm network chaos without bit flips (corruption is its own
//! campaign: [`crate::without_corruption`]) and knob spikes, plus a script
//! ([`QualificationFaults`]) of crashes, in-place and graceful reboots,
//! cuts, UTC steps and losses, key rotations; the reboot-storm variant
//! restarts one server at least ten times.
//!
//! **Replay.** A run's record ([`QualificationRecord`]) holds the
//! workload's semantic history, the ledger digest and a trace of the
//! campaign's own events captured from the simulation's timeline: two runs
//! of one seed must be equal, whether other seeds ran in between and
//! whatever the host's speed ([`HostJitter`]).

mod faults;
mod host;
mod legacy;
pub mod messages;
mod ops;
mod recovery;
mod server;
pub mod state;
mod third;
mod workload;

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_rpc::RpcConfig;
use moonpool_sim::{
    Chaos, ChaosMode, Invariant, SimulationBuilder, TraceQuery, WorkloadCount, assert_always,
    assert_sometimes,
};

pub use faults::QualificationFaults;
pub use host::HostJitter;
pub use legacy::Legacy;
pub use server::QualServer;
pub use third::Third;
pub use workload::{QualOp, QualificationConfig, QualificationWorkload};

use crate::foundations::state::Board;
pub(crate) use crate::foundations::{CALL_TIMEOUT, RPC_PORT};
pub(crate) use crate::streams::pause;

/// How long after the fault script stopped every service must be back:
/// the longest reconnect backoff the knobs allow (3 s), a failed session's
/// ping timeout (6 s), the balancer's longest exclusion (2 s) and its
/// all-alternatives probe wait, a directory lookup and a stream per server
/// on a fresh session, with margin.
pub const RECOVERY_BOUND: Duration = Duration::from_secs(30);

/// How long a stream whose consumer vanished may outlive it: the capped
/// inbound idle timeout (at most 12 s with the knobs) plus a ping timeout
/// (at most 6 s), with margin.
pub const RELEASE_BOUND: Duration = Duration::from_secs(30);

/// Workload lanes running the operation mix concurrently.
pub const LANES: usize = 3;

/// The chaos window.
pub const CHAOS: Duration = Duration::from_secs(20);

/// The event name of the campaign's own trace events.
pub const TRACE_EVENT: &str = "rpc_qual_event";

/// Events the semantic trace keeps, beside the campaign's own: the
/// transport's audit and shutdown events.
const TRACED: [&str; 3] = [TRACE_EVENT, "rpc_request_denied", "rpc_shutdown_started"];

/// One finished run as the workload saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationRecord {
    /// One line per operation: id, operation, target, outcome.
    pub history: Vec<String>,
    /// The ledgers as sorted lines (boots, publications, executions).
    pub ledger: Vec<String>,
    /// The campaign's and the transport's trace events, in timeline order,
    /// with their sim time, source and fields.
    pub trace: Vec<String>,
    /// Sim milliseconds from the end of the faults to full recovery (`None`:
    /// not recovered).
    pub recovered_ms: Option<u64>,
    /// Every runtime of the run was back at its baseline at the end.
    pub baseline: bool,
}

/// Where finished runs are appended, for tests to inspect.
pub type QualificationRecords = Arc<Mutex<Vec<QualificationRecord>>>;

/// The runtime configuration every process and the workload start from:
/// the streams campaign's buggified peer policy and squeezed budgets,
/// with the inbound idle timeout capped so a vanished caller's streams
/// are released within [`RELEASE_BOUND`].
#[must_use]
pub fn qual_config() -> RpcConfig {
    let mut config = crate::streams::streams_config();
    let floor = config.peer.ping_interval * 4;
    config.peer.inbound_idle_timeout = config
        .peer
        .inbound_idle_timeout
        .min(Duration::from_secs(8))
        .max(floor);
    config
}

/// Check that every earlier runtime of this process returned to its
/// baseline; returns this boot's board label.
pub(crate) fn check_previous_boots(board: &Board, role: &str, me: &str, boot: u64) -> String {
    let prefix = format!("{role}@{me}#");
    let label = format!("{prefix}{boot:04}");
    for (old, probe) in board.probes(&prefix, &label) {
        assert_always!(
            probe.is_at_baseline(),
            "rpc qual a restarted process's old runtime is at baseline",
            {
                "runtime" => old,
                "alive" => probe.runtime_alive(),
                "tasks" => probe.live_tasks(),
                "connections" => probe.live_connections(),
                "outstanding" => format!("{:?}", probe.outstanding())
            }
        );
    }
    if boot > 10 {
        assert_sometimes!(
            true,
            "rpc qual a tenth reboot still found its runtimes at baseline"
        );
    }
    label
}

/// Captures the campaign's trace events from the simulation's timeline.
struct TraceCapture {
    cursors: [Cell<usize>; TRACED.len()],
    sink: Arc<Mutex<Vec<String>>>,
}

impl Invariant for TraceCapture {
    fn name(&self) -> &'static str {
        "rpc_qual_trace_capture"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut events = Vec::new();
        for (name, cursor) in TRACED.iter().zip(&self.cursors) {
            events.extend(q.since(name, cursor));
        }
        events.sort_by_key(|event| event.seq);
        let mut sink = self
            .sink
            .lock()
            .expect("Mutex poisoned: prior task panicked");
        for event in events {
            sink.push(format!(
                "{} {} {} {:?}",
                event.time_ms, event.source, event.name, event.fields
            ));
        }
    }

    fn reset(&mut self) {
        for cursor in &self.cursors {
            cursor.set(0);
        }
        self.sink
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .clear();
    }
}

/// The campaign's builder without an iteration policy.
#[must_use]
pub fn qualification_campaign(
    config: QualificationConfig,
    records: &QualificationRecords,
) -> SimulationBuilder {
    let records = Arc::clone(records);
    let trace = Arc::new(Mutex::new(Vec::new()));
    let jitter = config.jitter;
    let storm = config.storm;
    let workload_trace = Arc::clone(&trace);
    SimulationBuilder::new()
        .processes(3, move || host::jittered(Box::new(QualServer), jitter))
        .processes(1, move || host::jittered(Box::new(Third), jitter))
        .processes(1, move || host::jittered(Box::new(Legacy), jitter))
        .workloads(WorkloadCount::Fixed(LANES), move |lane| {
            host::jittered_workload(
                Box::new(QualificationWorkload::new(
                    lane,
                    LANES,
                    config.clone(),
                    Arc::clone(&records),
                    Arc::clone(&workload_trace),
                )),
                jitter,
            )
        })
        .fault_factory(move || {
            Box::new(if storm {
                QualificationFaults::storm()
            } else {
                QualificationFaults::default()
            })
        })
        .invariant(TraceCapture {
            cursors: Default::default(),
            sink: trace,
        })
        .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::BuggifyKnobs])
        .network_fault_mask(crate::without_corruption())
        .chaos_duration(CHAOS)
}
