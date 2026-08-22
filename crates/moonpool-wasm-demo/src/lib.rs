//! Browser/wasm demo of one deterministic raw-TCP ping-pong simulation.
//!
//! The workload emits ordinary `client_*` tracing events. A separate invariant
//! reconstructs those events into [`Shot`]s, keeping visualization concerns out
//! of the actors. [`run_seed`] returns the structured result and
//! [`run_seed_json`] preserves the browser's JSON contract.

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
        .run();

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
