//! Write once, run in production, test under simulation.
//!
//! `fetch_with_retry` is ordinary application logic written against the
//! [`Providers`] traits — it never touches tokio directly. `main` runs it on the
//! real [`TokioProviders`] backend (wall-clock sleeps, OS RNG); the test at the
//! bottom drives the *same function* through [`SimulationBuilder`], where time is
//! logical and the RNG is seeded — so 50 deterministic "production days" run in
//! milliseconds.
//!
//! Run the production demo:    `cargo run --example retrying_worker`
//! Run the simulation test:    `cargo test --example retrying_worker`

use moonpool::prelude::*;
use std::time::Duration;

/// Retry an operation with exponential backoff + jitter, using only provider
/// traits. Because it is generic over `P: Providers`, the exact same code runs
/// on Tokio in production and under deterministic simulation in tests.
async fn fetch_with_retry<P: Providers>(providers: &P, max_attempts: u32) -> Result<u32, String> {
    let time = providers.time();
    let random = providers.random();

    for attempt in 0..max_attempts {
        // Stand-in for a real fallible operation (network call, etc.): succeeds
        // ~60% of the time. In sim this draw comes from the seeded RNG.
        if random.random_bool(0.6) {
            return Ok(attempt);
        }

        // Exponential backoff with jitter — every sleep goes through the time
        // provider, so simulation advances logical time instead of really waiting.
        let base_ms = 10u64 << attempt;
        let jitter_ms = random.random_range(0..base_ms.max(1));
        time.sleep(Duration::from_millis(base_ms + jitter_ms))
            .await
            .map_err(|e| e.to_string())?;
    }

    Err(format!("gave up after {max_attempts} attempts"))
}

/// Production entry point: drive the worker on the real Tokio backend.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let providers = TokioProviders::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(fetch_with_retry(&providers, 5));
    println!("production run finished: {result:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Drives `fetch_with_retry` under simulation. The workload pulls the
    /// `SimProviders` bundle from the context and hands it to the very same
    /// generic function `main` calls.
    struct RetryWorkload;

    #[async_trait]
    impl Workload for RetryWorkload {
        fn name(&self) -> &'static str {
            "retry-client"
        }

        async fn run(&mut self, ctx: &SimContext) -> moonpool::SimulationResult<()> {
            // Either outcome is fine — we are asserting the worker never panics
            // or hangs under simulated time + chaos, across many seeds.
            let _ = fetch_with_retry(ctx.providers(), 5).await;
            Ok(())
        }
    }

    #[test]
    fn survives_chaos() {
        let report = SimulationBuilder::new()
            .set_iterations(50)
            .workload(RetryWorkload)
            .run();
        assert_eq!(report.failed_runs, 0, "worker should survive every seed");
    }
}
