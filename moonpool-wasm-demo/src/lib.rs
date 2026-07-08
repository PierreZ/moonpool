//! Browser/wasm demo of moonpool: run **one deterministic seed** of a ping-pong
//! simulation over the real `moonpool-transport` RPC layer, driven by the
//! **simulated network**, and return the message timeline as JSON so a browser
//! can animate the exchange and the impact of chaos.
//!
//! The visualization is built **on top of the workload**, not inside it. The
//! workload ([`PingClient`]) is a plain transport client: it sends ping RPCs and
//! emits the *standard* observability events any well-instrumented client would
//! — `client_issued`, `client_acknowledged`, `client_failed` (each carrying a
//! `seq_id`). It contains zero visualization code.
//!
//! A generic [`TimelineRecorder`] — registered as an ordinary
//! [`moonpool_sim::Invariant`] — observes those `tracing` events from the sim's
//! timeline and reconstructs the message timeline ([`Shot`]s) plus the injected
//! network faults. Because it keys off the standard event contract, the *same*
//! recorder visualizes any transport workload that emits it (e.g. the
//! hash-chain workload in `moonpool-transport-sim`) with no changes.
//!
//! `.enable_chaos([Chaos::Network(ChaosMode::Random)])` turns on seeded network chaos — variable latency,
//! reordering, connection drops — so a request comes back fast, slowly, or not
//! at all. Each acknowledged request becomes two [`Shot`] legs (A→B then B→A)
//! split over the observed round-trip time; a failed request becomes a single
//! dropped A→B leg. Nothing really waits: logical time is driven by the sim
//! event queue, which is why the whole transport stack runs in a browser tab.
//!
//! Public entry points:
//! - [`run_seed`] — run a seed, get the structured [`RunResult`].
//! - [`run_seed_json`] — same, serialized to JSON (used by the native bin and,
//!   under `wasm32`, exported to JavaScript as `runSeed`).

use async_trait::async_trait;
use moonpool_sim::runner::builder::{ProcessCount, WorkloadCount};
use moonpool_sim::{
    Chaos, ChaosMode, Invariant, Process, SIM_FAULT_EVENT_NAME, SimContext, SimulationBuilder,
    SimulationError, SimulationResult, TimeProvider, TraceQuery, Workload, assert_always,
    assert_reachable, assert_sometimes,
};
use moonpool_transport::{
    Endpoint, JsonCodec, NetTransport, NetTransportBuilder, NetworkAddress, ReplyError,
    RequestEnvelope, UID, get_reply, make_decode_fn, make_encode_fn,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::wasm_bindgen;

// --- Tuning knobs ------------------------------------------------------------

/// Number of ping requests the client sends.
const REQUESTS: u32 = 12;
/// Per-request client deadline, in simulated milliseconds: past this, the
/// request is recorded as failed and the client moves on. Kept tight so a
/// slow/partitioned network produces visible drops, not just slow balls.
const TIMEOUT_MS: u64 = 700;
/// A round trip at least this slow (simulated ms) counts as "slowed by chaos".
const SLOW_RTT_MS: u64 = 100;
/// Minimum displayed flight time for a dropped leg, so it is always visible.
const MIN_DROP_SPAN_MS: u64 = 50;
/// How long the network-fault phase runs alongside the workload.
const CHAOS_SECS: u64 = 10;

/// Node A — the client (pinger).
const NODE_A: u8 = 0;
/// Node B — the server (ponger).
const NODE_B: u8 = 1;

/// RPC interface id for the ping service.
const PING_INTERFACE: u64 = 0xF00D_0001;
/// Method index for `ping` on [`PING_INTERFACE`].
const METHOD_PING: u64 = 1;

// Standard transport-client observability events the recorder consumes. Same
// names/field as `moonpool-transport-sim`, so the recorder is workload-agnostic.
const EV_ISSUED: &str = "client_issued";
const EV_ACKED: &str = "client_acknowledged";
const EV_FAILED: &str = "client_failed";
const EV_AMBIGUOUS: &str = "client_ambiguous";

/// A ping request: just a sequence number the server echoes back.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ping {
    /// Request sequence number.
    seq: u64,
}

/// A pong reply: the echoed sequence number.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pong {
    /// Echoed sequence number.
    seq: u64,
}

/// How a shot (one leg of a round trip) resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The leg was delivered over the network.
    Delivered,
    /// The request was dropped/timed out by chaos and never came back.
    Dropped,
}

/// One message crossing the simulated network: who sent it, when it left and
/// arrived (simulated milliseconds), how long it was in flight, and whether it
/// made it.
#[derive(Debug, Clone, Serialize)]
pub struct Shot {
    /// Request sequence number this leg belongs to.
    pub seq: u64,
    /// Node that sent this message (0 = A/client, 1 = B/server).
    pub from: u8,
    /// Node the message travels to (0 = A/client, 1 = B/server).
    pub to: u8,
    /// Simulated time the message left `from`, in milliseconds.
    pub depart_ms: u64,
    /// Simulated time the message reached `to`, in milliseconds.
    pub arrive_ms: u64,
    /// In-flight latency, in milliseconds (`arrive_ms - depart_ms`).
    pub latency_ms: u64,
    /// Whether this leg was delivered or dropped by chaos.
    pub outcome: Outcome,
}

/// The full result of one seeded run: every message leg, plus headline counters
/// the UI shows alongside the animation.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    /// The seed this run used.
    pub seed: u64,
    /// Number of ping requests observed.
    pub requests: u32,
    /// Every message leg exchanged, in time order.
    pub shots: Vec<Shot>,
    /// Round trips that completed successfully.
    pub delivered: u32,
    /// Requests dropped / timed out by chaos.
    pub dropped: u32,
    /// Network faults the sim injected (connection drops, etc.).
    pub faults: u32,
    /// Slowest successful round trip, in simulated milliseconds.
    pub longest_rtt_ms: u64,
    /// Total simulated time elapsed, in milliseconds.
    pub sim_duration_ms: u64,
}

impl RunResult {
    /// An empty result, used only if the run produced no observable events.
    fn empty(seed: u64) -> Self {
        Self {
            seed,
            requests: 0,
            shots: Vec::new(),
            delivered: 0,
            dropped: 0,
            faults: 0,
            longest_rtt_ms: 0,
            sim_duration_ms: 0,
        }
    }
}

/// UID for the ping method.
fn ping_method_uid() -> UID {
    UID::new(PING_INTERFACE, METHOD_PING)
}

/// Parse a sim IP (which may lack a port) into a [`NetworkAddress`], defaulting
/// to port 4500 (the sim convention).
fn parse_sim_addr(ip: &str) -> SimulationResult<NetworkAddress> {
    let addr_str = if ip.contains(':') {
        ip.to_string()
    } else {
        format!("{ip}:4500")
    };
    NetworkAddress::parse(&addr_str)
        .map_err(|e| SimulationError::InvalidState(format!("bad addr: {e}")))
}

/// Convert a [`Duration`] of simulated time to whole milliseconds.
fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ============================================================================
// Workload + server: plain transport actors. No visualization code lives here —
// they only do their job and emit standard observability events.
// ============================================================================

/// Node B: the ponger. Listens on its sim address, registers a ping handler, and
/// echoes every request's sequence number until shutdown.
struct PongServer;

#[async_trait]
impl Process for PongServer {
    fn name(&self) -> &'static str {
        "pong-server"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let addr = parse_sim_addr(ctx.my_ip())?;
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(addr)
            .build_listening()
            .await
            .map_err(|e| SimulationError::InvalidState(format!("server transport: {e}")))?;

        let (ping_stream, _) = NetTransport::register_handler_at::<Ping, Pong>(
            &transport,
            PING_INTERFACE,
            METHOD_PING,
        );

        let shutdown = ctx.shutdown().clone();
        loop {
            moonpool_sim::select! {
                biased;
                Some((req, reply)) = ping_stream.recv() => {
                    reply.send(Pong { seq: req.seq });
                }
                () = shutdown.cancelled() => return Ok(()),
            }
        }
    }
}

/// Node A: the pinger. Sends `REQUESTS` ping RPCs to the server through the
/// chaotic sim network and emits `client_issued` / `client_acknowledged` /
/// `client_failed` for each — the standard observability contract the recorder
/// reconstructs the timeline from. It has no knowledge of the visualization.
struct PingClient;

#[async_trait]
impl Workload for PingClient {
    fn name(&self) -> &'static str {
        "ping-client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips().to_vec();
        let Some(server_ip) = servers.first().cloned() else {
            return Ok(());
        };

        let my_addr = parse_sim_addr(ctx.my_ip())?;
        let transport = NetTransportBuilder::new(ctx.providers().clone())
            .local_address(my_addr)
            .build_listening()
            .await
            .map_err(|e| SimulationError::InvalidState(format!("client transport: {e}")))?;

        let endpoint = Endpoint::new(parse_sim_addr(&server_ip)?, ping_method_uid());
        let encode = make_encode_fn::<RequestEnvelope<Ping>, _>(JsonCodec);
        let decode = make_decode_fn::<Result<Pong, ReplyError>, _>(JsonCodec);

        let time = ctx.time().clone();
        let shutdown = ctx.shutdown().clone();

        for seq in 0..u64::from(REQUESTS) {
            if shutdown.is_cancelled() {
                break;
            }

            let send = time.now();
            tracing::info!(seq_id = seq, "client_issued");

            // Reliable RPC, abandoned if it doesn't return within the deadline.
            let result: Option<Result<Pong, ReplyError>> = match get_reply(
                &*transport,
                &endpoint,
                Ping { seq },
                &encode,
                decode.clone(),
            ) {
                Ok(fut) => moonpool_sim::select! {
                    biased;
                    r = fut => Some(r),
                    () = shutdown.cancelled() => None,
                    _ = time.sleep(Duration::from_millis(TIMEOUT_MS)) => None,
                },
                Err(_) => None,
            };

            if let Some(Ok(pong)) = result {
                assert_always!(pong.seq == seq, "pong echoes the ping it answered");
                let rtt_ms = duration_ms(time.now().saturating_sub(send));
                assert_sometimes!(rtt_ms >= SLOW_RTT_MS, "a round trip is slowed by chaos");
                tracing::info!(seq_id = seq, "client_acknowledged");
            } else {
                assert_reachable!("chaos drops a request before any pong returns");
                tracing::info!(seq_id = seq, "client_failed");
            }
        }
        Ok(())
    }
}

// ============================================================================
// Visualization layer: generic, sits on top of any workload via the timeline.
// ============================================================================

/// Raw timeline the recorder accumulates from the sim's trace events.
#[derive(Default)]
struct RecorderData {
    /// `(seq_id, sim_time_ms)` for each issued request.
    issued: Vec<(u64, u64)>,
    /// `(seq_id, sim_time_ms)` for each acknowledged (delivered) request.
    acked: Vec<(u64, u64)>,
    /// `(seq_id, sim_time_ms)` for each failed/ambiguous (dropped) request.
    failed: Vec<(u64, u64)>,
    /// `(sim_time_ms, kind)` for each injected network fault.
    faults: Vec<(u64, String)>,
}

/// Pull `(seq_id, time_ms)` pairs for every event named `name`.
fn collect_seq(q: &dyn TraceQuery, name: &str) -> Vec<(u64, u64)> {
    q.snapshot(name)
        .into_iter()
        .filter_map(|e| Some((e.u64("seq_id")?, e.time_ms)))
        .collect()
}

/// A workload-agnostic recorder. As an [`Invariant`] it sees the whole trace
/// timeline after each step; it snapshots the standard client events and the
/// injected faults into shared state the driver reads once the run completes.
struct TimelineRecorder {
    data: Arc<Mutex<RecorderData>>,
}

impl Invariant for TimelineRecorder {
    fn name(&self) -> &'static str {
        "timeline_recorder"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut d = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        d.issued = collect_seq(q, EV_ISSUED);
        d.acked = collect_seq(q, EV_ACKED);
        let mut failed = collect_seq(q, EV_FAILED);
        failed.extend(collect_seq(q, EV_AMBIGUOUS));
        d.failed = failed;
        d.faults = q
            .snapshot(SIM_FAULT_EVENT_NAME)
            .into_iter()
            .map(|e| (e.time_ms, e.str("kind").unwrap_or("fault").to_string()))
            .collect();
    }
}

/// Turn the recorded timeline into the animation [`RunResult`]: match each
/// issued request to its acknowledgement (delivered) or failure (dropped), and
/// synthesize the two legs of every delivered round trip.
fn build_result(seed: u64, data: &RecorderData) -> RunResult {
    let ack: HashMap<u64, u64> = data.acked.iter().copied().collect();
    let fail: HashMap<u64, u64> = data.failed.iter().copied().collect();
    let mut issued = data.issued.clone();
    issued.sort_by_key(|&(_, t)| t);

    let mut shots = Vec::new();
    let mut delivered = 0_u32;
    let mut dropped = 0_u32;
    let mut longest_rtt_ms = 0_u64;

    for (seq, issue_ms) in issued.iter().copied() {
        if let Some(&ack_ms) = ack.get(&seq) {
            delivered += 1;
            let rtt = ack_ms.saturating_sub(issue_ms);
            longest_rtt_ms = longest_rtt_ms.max(rtt);
            let mid_ms = issue_ms.saturating_add(rtt / 2);
            shots.push(Shot {
                seq,
                from: NODE_A,
                to: NODE_B,
                depart_ms: issue_ms,
                arrive_ms: mid_ms,
                latency_ms: mid_ms.saturating_sub(issue_ms),
                outcome: Outcome::Delivered,
            });
            shots.push(Shot {
                seq,
                from: NODE_B,
                to: NODE_A,
                depart_ms: mid_ms,
                arrive_ms: ack_ms,
                latency_ms: ack_ms.saturating_sub(mid_ms),
                outcome: Outcome::Delivered,
            });
        } else {
            dropped += 1;
            let end_ms = fail.get(&seq).copied().unwrap_or(issue_ms);
            let span = end_ms.saturating_sub(issue_ms).max(MIN_DROP_SPAN_MS);
            shots.push(Shot {
                seq,
                from: NODE_A,
                to: NODE_B,
                depart_ms: issue_ms,
                arrive_ms: issue_ms.saturating_add(span),
                latency_ms: span,
                outcome: Outcome::Dropped,
            });
        }
    }

    let mut sim_duration_ms = shots.iter().map(|s| s.arrive_ms).max().unwrap_or(0);
    for (t, _) in &data.faults {
        sim_duration_ms = sim_duration_ms.max(*t);
    }

    RunResult {
        seed,
        requests: u32::try_from(issued.len()).unwrap_or(u32::MAX),
        shots,
        delivered,
        dropped,
        faults: u32::try_from(data.faults.len()).unwrap_or(u32::MAX),
        longest_rtt_ms,
        sim_duration_ms,
    }
}

/// Run one deterministic seed of the ping-pong transport simulation and return
/// its full timeline. The same seed always produces the same [`RunResult`].
#[must_use]
pub fn run_seed(seed: u64) -> RunResult {
    let data = Arc::new(Mutex::new(RecorderData::default()));
    let _report = SimulationBuilder::new()
        .processes(ProcessCount::Fixed(1), || Box::new(PongServer))
        .workloads(WorkloadCount::Fixed(1), |_| Box::new(PingClient))
        .invariant(TimelineRecorder { data: data.clone() })
        .chaos_duration(Duration::from_secs(CHAOS_SECS))
        .enable_chaos([Chaos::Network(ChaosMode::Random)])
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run();

    let data = data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if data.issued.is_empty() {
        return RunResult::empty(seed);
    }
    build_result(seed, &data)
}

/// Run one seed and serialize the [`RunResult`] to a JSON string. Serializing a
/// plain data struct cannot fail, but on the off chance it does the error is
/// returned as a small JSON object instead of panicking.
#[must_use]
pub fn run_seed_json(seed: u64) -> String {
    serde_json::to_string(&run_seed(seed))
        .unwrap_or_else(|e| format!("{{\"error\":\"serialize failed: {e}\"}}"))
}

/// wasm entry point exported to JavaScript as `runSeed(seed)`. Installs the
/// panic hook first so any runtime panic (e.g. a `block_on` that parks) surfaces
/// as a real message in the browser console rather than an opaque trap.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = runSeed)]
#[must_use]
pub fn run_seed_wasm(seed: u64) -> String {
    console_error_panic_hook::set_once();
    run_seed_json(seed)
}

#[cfg(test)]
mod tests {
    use super::{REQUESTS, run_seed, run_seed_json};

    #[test]
    fn runs_and_is_reproducible() {
        let first = run_seed_json(42);
        let second = run_seed_json(42);
        assert_eq!(first, second, "same seed must reproduce the same timeline");

        let result = run_seed(42);
        assert_eq!(
            result.requests, REQUESTS,
            "every request should be observed"
        );
        assert!(
            !result.shots.is_empty(),
            "a seeded run should exchange messages"
        );
        for shot in &result.shots {
            assert!(
                shot.arrive_ms >= shot.depart_ms,
                "a message arrived before it left"
            );
        }
        assert_eq!(
            result.delivered + result.dropped,
            REQUESTS,
            "every request is either delivered or dropped"
        );
    }

    #[test]
    fn distinct_seeds_differ() {
        // Guards against a constant / no-chaos bug: two seeds producing identical
        // timelines would be astronomically unlikely with seeded network chaos.
        assert_ne!(run_seed_json(7), run_seed_json(99));
    }
}
