//! Native smoke runner: `cargo run -p moonpool-wasm-demo [seed]` runs one seed
//! of the ping-pong transport simulation and prints its message timeline, so the
//! workload is verified on native hardware before any wasm build. The browser
//! calls the exact same [`moonpool_wasm_demo::run_seed`] under the hood.

use moonpool_wasm_demo::{Outcome, run_seed, run_seed_json};

fn main() {
    let seed = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(42);

    let result = run_seed(seed);

    println!(
        "seed {} — {} requests over the simulated network: {} delivered, {} dropped by chaos, \
         {} net faults, slowest RTT {} ms, {} ms simulated\n",
        result.seed,
        result.requests,
        result.delivered,
        result.dropped,
        result.faults,
        result.longest_rtt_ms,
        result.sim_duration_ms,
    );

    for shot in &result.shots {
        let arrow = if shot.from == 0 { "A → B" } else { "B → A" };
        let mark = match shot.outcome {
            Outcome::Delivered => "delivered",
            Outcome::Dropped => "✗ DROPPED",
        };
        println!(
            "  req {:>2}  {}  {:>4}ms  (t={:>5}ms)  {}",
            shot.seq, arrow, shot.latency_ms, shot.arrive_ms, mark,
        );
    }

    // Print the JSON the browser would receive, so the wire format is eyeballable.
    println!("\n--- JSON (what runSeed returns to the browser) ---");
    println!("{}", run_seed_json(seed));
}
