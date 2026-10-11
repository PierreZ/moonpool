//! Browser demo of one deterministic raw-TCP ping-pong simulation.
//!
//! The crate runs **one seed** of a small simulation and returns its message
//! timeline. The same code runs natively and as wasm in a browser tab, where
//! `web/index.html` animates each round trip and the impact of network
//! chaos. The Sim Book embeds this page. The crate is not published.
//!
//! Two nodes speak a tiny integrity-checked TCP protocol:
//!
//! - **Node B** is a server `Process` (the ponger) that echoes every frame.
//! - **Node A** is a client `Workload` (the pinger) that sends 12 frames.
//!
//! `Chaos::Network(ChaosMode::Random)` adds seeded latency, reordering and
//! connection drops. The global scheduler advances logical time through
//! delayed network events, so nothing really waits, and a whole run fits in
//! a browser tab. The same seed gives the same run, byte for byte, natively
//! and in the browser.
//!
//! # Quick example
//!
//! ```rust
//! use moonpool_wasm_demo::{run_seed, run_seed_json};
//!
//! let result = run_seed(42);
//! assert_eq!(result.delivered + result.dropped, result.requests);
//! assert!(result.shots.iter().all(|shot| shot.arrive_ms >= shot.depart_ms));
//!
//! // The browser receives this JSON; the same seed gives the same bytes.
//! assert_eq!(run_seed_json(42), run_seed_json(42));
//! ```
//!
//! # Features
//!
//! - **Observation on top, not inside.** The workload emits only the
//!   ordinary `client_*` tracing events any instrumented client would. A
//!   separate `Invariant` reads them from the simulation timeline (through
//!   `TraceQuery`, with the injected faults under `SIM_FAULT_EVENT_NAME`)
//!   and rebuilds the [`Shot`]s. The actors hold no visualization code.
//! - **Stable JSON model.** [`run_seed`] returns a [`RunResult`];
//!   [`run_seed_json`] serializes it for the browser.
//! - **wasm entry point.** On `wasm32`, `runSeed(seed)` is exported through
//!   `wasm-bindgen` and installs `console_error_panic_hook`.
//!
//! # Feature flags
//!
//! None. The crate depends on `moonpool-sim` with `default-features =
//! false`: no tokio providers and no fork-based exploration, which is the
//! configuration that builds for `wasm32-unknown-unknown`.

mod actors;
mod model;
mod protocol;
mod timeline;

use moonpool_sim::runner::builder::{ProcessCount, WorkloadCount};
use moonpool_sim::{Chaos, ChaosMode, SimulationBuilder};

pub use model::{Outcome, RunResult, Shot};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::wasm_bindgen;

/// Run one deterministic seed and return its complete animation timeline.
///
/// # Panics
///
/// Panics if the demo's fixed builder configuration is invalid, which is a
/// bug in this crate rather than something a seed can cause.
#[must_use]
pub fn run_seed(seed: u64) -> RunResult {
    let (data, recorder) = timeline::recorder();
    let _report = SimulationBuilder::new()
        .processes(ProcessCount::Fixed(1), || Box::new(actors::PongServer))
        .workloads(WorkloadCount::Fixed(1), |_| Box::new(actors::PingClient))
        .invariant(recorder)
        .chaos_duration(actors::CHAOS_DURATION)
        .enable_chaos([Chaos::Network(ChaosMode::Random)])
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run()
        .expect("simulation configuration is valid");

    timeline::finish(seed, &data)
}

/// Run one seed and serialize its [`RunResult`] for the browser.
#[must_use]
pub fn run_seed_json(seed: u64) -> String {
    serde_json::to_string(&run_seed(seed))
        .unwrap_or_else(|error| format!("{{\"error\":\"serialize failed: {error}\"}}"))
}

/// JavaScript entry point exported as `runSeed(seed)` on wasm.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = runSeed)]
#[must_use]
pub fn run_seed_wasm(seed: u64) -> String {
    console_error_panic_hook::set_once();
    run_seed_json(seed)
}

#[cfg(test)]
mod tests {
    use super::{actors::REQUESTS, protocol, run_seed, run_seed_json};

    #[test]
    fn frame_roundtrips_and_detects_corruption() {
        let mut frame = protocol::encode_frame(42);
        assert_eq!(protocol::decode_frame(&frame), Some(42));

        frame[3] ^= 1;
        assert_eq!(protocol::decode_frame(&frame), None);
    }

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
        assert!(
            result
                .shots
                .iter()
                .all(|shot| shot.arrive_ms >= shot.depart_ms),
            "a message arrived before it left"
        );
        assert_eq!(
            result.delivered + result.dropped,
            REQUESTS,
            "every request is either delivered or dropped"
        );
    }

    #[test]
    fn distinct_seeds_differ() {
        assert_ne!(run_seed_json(7), run_seed_json(99));
    }

    #[test]
    fn json_contract_keeps_the_browser_fields() {
        let value: serde_json::Value =
            serde_json::from_str(&run_seed_json(42)).expect("run result is valid JSON");
        let result = value.as_object().expect("run result is a JSON object");
        for field in [
            "seed",
            "requests",
            "shots",
            "delivered",
            "dropped",
            "faults",
            "longest_rtt_ms",
            "sim_duration_ms",
        ] {
            assert!(result.contains_key(field), "missing result field {field}");
        }

        let shot = result["shots"]
            .as_array()
            .and_then(|shots| shots.first())
            .and_then(serde_json::Value::as_object)
            .expect("seed 42 produces at least one shot");
        for field in [
            "seq",
            "from",
            "to",
            "depart_ms",
            "arrive_ms",
            "latency_ms",
            "outcome",
        ] {
            assert!(shot.contains_key(field), "missing shot field {field}");
        }
    }
}
