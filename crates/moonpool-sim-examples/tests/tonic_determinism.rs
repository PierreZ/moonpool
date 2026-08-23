//! Cross-run determinism regression for tonic over `ReconnectingChannel`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, Chaos, ChaosMode, SIM_FAULT_EVENT_NAME, SimulationBuilder,
};
use moonpool_sim_examples::tonic_grpc::{EchoProcess, EchoWorkload};

type TraceEntry = (u64, u64, String, String, String);

const EVENT_NAMES: &[&str] = &[
    "grpc server listening",
    "accepted connection",
    "grpc server shutting down",
    "workload starting",
    "starting round",
    "round completed successfully",
    "workload finished all rounds",
    "echo_served",
    "echo_stream_started",
    "stream completed",
    "stream aborted (buggified)",
    "calling unmounted Shout service",
    "unmounted service correctly rejected",
    SIM_FAULT_EVENT_NAME,
];

fn run_once(seed: u64) -> Vec<TraceEntry> {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&trace);

    let report = SimulationBuilder::new()
        .processes(1, || Box::new(EchoProcess))
        .workload(EchoWorkload)
        .invariant_fn("tonic replay trace", move |query, _| {
            let mut events = EVENT_NAMES
                .iter()
                .flat_map(|name| query.snapshot(name))
                .map(|event| {
                    (
                        event.seq,
                        event.time_ms,
                        event.source,
                        event.name,
                        format!("{:?}", event.fields),
                    )
                })
                .collect::<Vec<_>>();
            events.sort_by_key(|event| event.0);
            *captured
                .lock()
                .expect("Mutex poisoned: prior test panicked") = events;
        })
        .enable_chaos([
            Chaos::Network(ChaosMode::Random),
            Chaos::Attrition {
                config: Attrition {
                    max_dead: 1,
                    prob_graceful: 0.3,
                    prob_crash: 0.5,
                    prob_wipe: 0.2,
                    recovery_delay_ms: None,
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                },
                mode: ChaosMode::Random,
            },
        ])
        .chaos_duration(Duration::from_secs(10))
        .set_debug_seeds(vec![seed])
        .set_iterations(1)
        .run();
    assert_eq!(report.failed_runs, 0, "seed {seed} must succeed");

    let events = trace
        .lock()
        .expect("Mutex poisoned: prior test panicked")
        .clone();
    assert!(!events.is_empty(), "seed {seed} must emit trace events");
    events
}

#[test]
fn tonic_replay_is_independent_of_earlier_in_process_runs() {
    for seed in [1_u64, 42] {
        assert_eq!(run_once(seed), run_once(seed), "warm-up seed {seed}");
    }

    assert_eq!(run_once(12_345), run_once(12_345), "target seed");
}
