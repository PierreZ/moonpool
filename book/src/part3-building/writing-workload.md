# Writing a Workload

<!-- toc -->

The workload drives our key-value server and checks that it behaves correctly. It sends requests, tracks expected state in a reference model, and uses assertions to catch bugs as they happen.

## The Workload Struct

Unlike Processes, workloads accumulate state across the entire simulation. The struct holds everything the workload needs to track:

```rust
use async_trait::async_trait;
use moonpool_sim::{
    SimContext, SimulationResult, Workload,
    assert_always, assert_sometimes,
};

struct KvWorkload {
    /// Number of operations per run
    num_ops: usize,
    /// Reference model: what we expect the server to contain
    model: HashMap<String, Vec<u8>>,
    /// Keys we use for operations
    keys: Vec<String>,
}
```

The `model` is the most important field. It mirrors what the server should contain. Every time we write to the server, we write the same value to the model. Every time we read from the server, we compare the result against the model.

## Setup: Finding the Servers

The `setup()` method runs before any workload's `run()` starts. Use it to locate processes and prepare connections:

```rust
#[async_trait(?Send)]
impl Workload for KvWorkload {
    fn name(&self) -> &str {
        "kv-client"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Verify we have servers to talk to
        let process_ips = ctx.topology().all_process_ips();
        if process_ips.is_empty() {
            return Err(moonpool_sim::SimulationError::InvalidState(
                "no server processes available".into(),
            ));
        }
        self.model.clear();
        Ok(())
    }
```

The key discovery mechanism is `ctx.topology()`. It knows about every participant in the simulation: which IPs are processes, which are workloads, what tags each process has.

Common patterns for finding servers:

```rust
// All server IPs
let all_servers = ctx.topology().all_process_ips();

// A specific peer by name
let server_ip = ctx.peer("server").expect("server exists");

// Servers with a particular role tag
let leaders = ctx.topology().ips_tagged("role", "leader");
```

## Run: The Operation Loop

The `run()` method is where bugs get found. We generate random operations, execute them against the server, and verify each response:

```rust
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let server_ips = ctx.topology().all_process_ips().to_vec();

        for i in 0..self.num_ops {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            // Pick a random server
            let server_idx = ctx.random().random_range(0..server_ips.len());
            let server_ip = &server_ips[server_idx];

            // Generate a random operation
            let roll = ctx.random().random_range(0..100);
            match roll {
                0..40 => {
                    // SET operation
                    let key = &self.keys[
                        ctx.random().random_range(0..self.keys.len())
                    ];
                    let value = format!("v{}", i).into_bytes();

                    match self.send_set(ctx, server_ip, key, &value).await {
                        Ok(()) => {
                            self.model.insert(key.clone(), value);
                            assert_sometimes!(true, "set_succeeded");
                        }
                        Err(e) => {
                            tracing::warn!("set failed: {}", e);
                            assert_sometimes!(true, "set_failed_network");
                        }
                    }
                }
                40..80 => {
                    // GET operation
                    let key = &self.keys[
                        ctx.random().random_range(0..self.keys.len())
                    ];

                    match self.send_get(ctx, server_ip, key).await {
                        Ok(value) => {
                            let expected = self.model.get(key)
                                .cloned()
                                .unwrap_or_default();
                            assert_always!(
                                value == expected,
                                format!(
                                    "read mismatch for '{}': got {} bytes, expected {}",
                                    key, value.len(), expected.len()
                                )
                            );
                        }
                        Err(e) => {
                            tracing::warn!("get failed: {}", e);
                        }
                    }
                }
                _ => {
                    // Small delay to let simulation events process
                    let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Ok(())
    }
```

Notice the two assertion types working together:

**`assert_always!`** guards invariants that must never be violated. If a GET returns data that does not match our model, something is broken. An always-assertion failure is a definite bug.

**`assert_sometimes!`** marks paths that should fire at least once across all iterations. If `"set_succeeded"` never triggers across hundreds of seeds, something is wrong with our test setup. If `"set_failed_network"` never triggers, we might not have enough chaos.

## Check: Final Validation

After all workloads finish and pending events drain, `check()` runs for final state validation:

```rust
    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        // Verify model consistency
        let total_keys = self.model.len();
        assert_always!(
            total_keys <= self.keys.len(),
            format!(
                "model has more keys than expected: {} > {}",
                total_keys, self.keys.len()
            )
        );

        Ok(())
    }
}
```

The check phase is your last chance to validate. The system is quiet. No messages in flight, no operations pending. What the model says should match what the server actually contains.

## Publishing State for Invariants

Workloads can publish state that invariant functions read after every simulation event. This enables cross-workload validation:

```rust
// In run(), after each operation:
ctx.state().publish("kv_model", self.model.clone());
```

An invariant function (registered on the builder) can then read this state:

```rust
fn check_model_size(state: &StateHandle, _sim_time_ms: u64) {
    if let Some(model) = state.get::<HashMap<String, Vec<u8>>>("kv_model") {
        assert_always!(
            model.len() <= 100,
            "model grew beyond expected bounds"
        );
    }
}
```

This is how moonpool's banking simulation validates the conservation law: the workload publishes the reference model, and the invariant checks `sum(balances) == deposits - withdrawals` after every event.

## Patterns That Find Bugs

The strongest workloads combine several techniques:

**Reference model**: Track expected state locally. Compare against actual server responses. Any divergence is a bug.

**Weighted operation alphabet**: Mix writes, reads, and delays. Control the distribution. Too predictable means you only test happy paths.

**Both assertion types**: `assert_always!` for correctness properties that must hold on every single call. `assert_sometimes!` for coverage goals that should fire at least once across the full run.

**Handle failures gracefully**: Network errors during chaos are expected. Log them, maybe track them in the model, but do not treat them as test failures. The bug is when the server returns the **wrong** answer, not when it returns an error.
