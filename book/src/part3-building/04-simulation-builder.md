# Configuring the SimulationBuilder

<!-- toc -->

The `SimulationBuilder` is the glue. It takes your Process, your Workload, your invariants, and your chaos configuration, and wires them into a runnable simulation.

## The Minimal Builder

The simplest possible simulation has one workload and runs once:

```rust
let report = SimulationBuilder::new()
    .workload(KvWorkload::new(100, keys))
    .run()
    .await;
```

This creates a single workload at IP `10.0.0.1`, runs it with a random seed, and produces a `SimulationReport`. No processes, no chaos, no multiple iterations. Useful for smoke testing, but not for finding bugs.

## Adding Processes

To test a client-server system, add processes alongside the workload:

```rust
let report = SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workload(KvWorkload::new(100, keys))
    .run()
    .await;
```

The builder creates 3 server processes at `10.0.1.1` through `10.0.1.3` and one workload at `10.0.0.1`. The workload finds server IPs through `ctx.topology().all_process_ips()`.

## Iteration Control

One iteration is not enough. Different seeds produce different scheduling orders, different random choices, different failure patterns. You need hundreds or thousands of iterations to find bugs hiding in rare interleavings.

**Fixed count** runs a specific number of iterations:

```rust
.set_iterations(100)
// or equivalently:
.set_iteration_control(IterationControl::FixedCount(100))
```

**Time limit** runs until a wall-clock deadline:

```rust
.set_time_limit(Duration::from_secs(60))
```

Each iteration gets a different seed, producing a different execution. The seeds are deterministic and derived from the iteration manager, so the same configuration always explores the same seeds.

## Seed Control

When a simulation fails on a specific seed, you need to reproduce it. Use `set_debug_seeds()` to run exactly those seeds:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workload(KvWorkload::new(100, keys))
    .set_debug_seeds(vec![42, 7891])
    .run()
    .await;
```

This runs exactly 2 iterations with seeds 42 and 7891. Combined with `RUST_LOG=error`, this is the primary debugging workflow: find the failing seed in the report, reproduce it in isolation, add logging, find the bug.

## Tags for Role Distribution

When your distributed system has roles, tags assign them to processes:

```rust
SimulationBuilder::new()
    .processes(5, || Box::new(ConsensusNode))
    .tags(&[
        ("role", &["leader", "follower"]),
        ("dc", &["east", "west", "eu"]),
    ])
```

Tags distribute round-robin. Process 0 gets `role=leader, dc=east`. Process 1 gets `role=follower, dc=west`. Process 2 gets `role=leader, dc=eu`. And so on, wrapping around.

Inside a Process, read tags via `ctx.topology().my_tags().get("role")`. Inside a Workload, query the tag registry: `ctx.topology().ips_tagged("role", "leader")` returns the IPs of all leader processes.

## Invariants

Invariants run after **every simulation event**. They check cross-workload properties that must hold at all times, not just at the end.

**Trait-based invariant**:

```rust
struct ConservationLaw;

impl Invariant for ConservationLaw {
    fn name(&self) -> &str { "conservation_law" }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<BankingModel>("banking_model") {
            let total = model.total_balance();
            let expected = model.total_deposited - model.total_withdrawn;
            assert_always!(total == expected, "conservation violated");
        }
    }
}

// Register on builder:
.invariant(ConservationLaw)
```

**Closure-based invariant** for simpler cases:

```rust
.invariant_fn("key_count_bounded", |state, _time| {
    if let Some(model) = state.get::<KvModel>("kv_model") {
        assert_always!(model.len() <= 1000, "too many keys");
    }
})
```

Invariants read from the `StateHandle`, which workloads write to via `ctx.state().publish()`. This is how the test driver communicates its reference model to the invariant checker.

## Chaos and Attrition

Real distributed systems do not just run cleanly. Servers crash, networks partition, and then things have to recover. The builder models this with `chaos_duration`:

```rust
use moonpool_sim::Attrition;

SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workload(KvWorkload::new(200, keys))
    .chaos_duration(Duration::from_secs(30))
    .attrition(Attrition {
        max_dead: 1,
        prob_graceful: 0.3,
        prob_crash: 0.5,
        prob_wipe: 0.2,
        recovery_delay_ms: Some(1000..5000),
        grace_period_ms: Some(2000..4000),
    })
    .set_iterations(100)
    .run()
    .await;
```

The simulation lifecycle:

1. **Chaos phase** (30 simulated seconds): Workloads run concurrently with fault injectors. Attrition randomly kills and restarts processes, respecting `max_dead` to avoid killing everything at once.

2. **Workload completion**: After chaos ends, faults stop and the system continues until all workloads finish. Workloads should be finite (do N operations, or sleep for a sim-time duration, then return).

3. **Settle**: The orchestrator drains remaining events. If the system does not settle within 30 seconds (sim time), the test fails with diagnostics, surfacing cleanup bugs like leaked tasks or unclosed connections.

4. **Check**: The `check()` methods run inside the event loop, so network RPCs work normally.

`max_dead: 1` means at most one process is down at any time. The probability weights control the mix of graceful shutdowns (shutdown token fired, grace period) versus instant crashes (no warning, connections abort).

## Randomized Network

For additional chaos, enable randomized network configuration:

```rust
.random_network()
```

This varies latency, packet delay distributions, and other network parameters per iteration, based on the seed. Without this flag, the network uses default configuration (consistent, low-latency).

## Putting It All Together

A production-grade simulation configuration looks like this:

```rust
let report = SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .tags(&[("role", &["primary", "replica"])])
    .workload(KvWorkload::new(500, keys.clone()))
    .invariant(ConservationLaw)
    .chaos_duration(Duration::from_secs(30))
    .attrition(Attrition {
        max_dead: 1,
        prob_graceful: 0.3,
        prob_crash: 0.5,
        prob_wipe: 0.2,
        recovery_delay_ms: None,
        grace_period_ms: None,
    })
    .random_network()
    .set_iterations(100)
    .run()
    .await;
```

The builder takes care of the rest: creating the simulated world, assigning IPs, seeding the RNG, running the orchestration loop, collecting metrics, and producing the report.
