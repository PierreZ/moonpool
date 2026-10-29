# Configuring the SimulationBuilder

<!-- toc -->

The `SimulationBuilder` is the glue. It takes your Process, your Workload, your invariants and your chaos configuration, and wires them into a runnable campaign. This chapter walks the calls our key-value simulation needs, then lists every public method with the chapter that explains it.

## The Minimal Builder

The smallest simulation has one workload and no processes.

```rust
let report = SimulationBuilder::new()
    .workload(KvWorkload::new(100))
    .run()?;
```

This places the workload at `10.0.0.1`, runs it under the default iteration control, and returns a `SimulationReport`. `run()` returns `Err(SimulationError::InvalidConfiguration)` when the builder cannot run as written: a fault injector or `Chaos::Attrition` without `chaos_duration`, or an instance workload under exploration or `check_determinism`. In that case no seed ran. Failing seeds are never an `Err`. They are in the report.

## Adding Processes

```rust
let report = SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workload(KvWorkload::new(100))
    .run()?;
```

Three servers at `10.0.1.1` to `10.0.1.3`, one client at `10.0.0.1`. The workload finds the servers through `ctx.topology().all_process_ips()`. Call `.processes()` once per **role**. Each call is a group on its own `10.0.{g+1}.x` range, and [Process Groups, Tags and Topology](./27-process-groups.md) covers groups, `.cluster()` and `.tags()`.

## Iteration Control

One seed is one schedule. Bugs hide in rare interleavings, so a campaign runs many seeds. `IterationControl` has two modes.

**`UntilCoverageStable`** is the default, with `plateau_seeds: 10` and `max_iterations: 1000`. It stops once every observed `assert_sometimes!` and `assert_reachable!` has fired **and** coverage has not grown for `plateau_seeds` consecutive seeds. Tune it with:

```rust
.until_coverage_stable(10, 5_000) // 10 quiet seeds, 5,000 safety cap
```

**`FixedCount`** runs exactly `n` seeds:

```rust
.set_iterations(100)
```

Generated seeds derive from a wall-clock base seed, so two campaigns explore different seeds. To replay, pin them:

```rust
.set_debug_seeds(vec![42, 7891])
.set_iterations(2)
```

`set_debug_seeds` supplies the first seeds. `set_iterations` bounds the count. Without the second call the default control keeps drawing fresh seeds after 42 and 7891. [Reproducing with FixedCount](./21-reproducing.md) is the debugging recipe.

## Invariants

Invariants run after **every simulation step** and read the captured tracing timeline. They catch cross-workload properties while the run is still going.

```rust
use std::cell::Cell;
use moonpool_sim::{Invariant, TraceQuery, assert_always};

struct SingleLeader { cursor: Cell<usize> }

impl Invariant for SingleLeader {
    fn name(&self) -> &str { "single_leader" }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        // A cursor scan: only the events since the last step, never the whole timeline.
        for e in q.since("leader_elected", &self.cursor) {
            assert_always!(e.u64("term").is_some(), "leader_elected carries a term");
        }
    }

    fn reset(&mut self) { self.cursor.set(0); }
}

// On the builder:
.invariant(SingleLeader { cursor: Cell::new(0) })
.invariant_fn("kv_commits_name_a_key", |q, _t| {
    for e in q.snapshot("kv_set_committed") {
        assert_always!(e.str("key").is_some(), "kv_set_committed carries a key");
    }
})
```

[Events and Invariants](./17-events-and-invariants.md) covers emission, capture and the query API.

## Chaos

`enable_chaos` takes a list of surfaces, each with a `ChaosMode`. `Random` samples the full surface every seed. `Swarm` enables a random subset of its fault families per seed and turns the rest fully off.

```rust
use moonpool_sim::{Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode};

SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .workload(KvWorkload::new(200))
    .chaos_duration(Duration::from_secs(30))
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::Attrition {
            config: Attrition {
                max_dead: 1,
                prob_graceful: 0.3,
                prob_crash: 0.5,
                prob_wipe: 0.2,
                recovery_delay_ms: None,
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
                wipe_budget: None,
            },
            mode: ChaosMode::Swarm,
        },
    ])
    .set_iterations(100)
    .run()?;
```

`chaos_duration` opens the window in which new faults may be injected. When it closes, the runner cancels the injectors and enters recovery mode: every configuration-driven fault family switches off and held partitions heal. Damage already done stays done. The quiet tail between the cutoff and workload completion is where the system under test recovers, and [Chaos in Moonpool](./07-chaos.md) describes that lifecycle. Each `Chaos` variant has its own chapter:

| Variant | Covers | Chapter |
|---|---|---|
| `Chaos::Network(mode)` | partitions, clogs, bit flips, random close, latency | [Network Faults](./10-network-faults.md) |
| `Chaos::Storage(mode)` | corruption, misdirection, lost writes, sync lies, stalls | [Storage Faults](./11-storage-faults.md) |
| `Chaos::Attrition { config, mode }` | graceful, crash and wipe reboots, one regime per entry | [Attrition](./09-attrition.md) |
| `Chaos::Outage { config, mode }` | every process of a group loses power at once | [Correlated Outages](./30-outages.md) |
| `Chaos::BuggifyKnobs` | push enabled network and storage knobs to extremes | [Buggify](./08-buggify.md) |
| `Chaos::Cpu(mode)` | every poll costs CPU time on the process's one core | [CPU Time and Gray Failures](./26-cpu-and-gray-failure.md) |
| `Chaos::GrayFailure { config, mode }` | machines that stay alive and slow | [CPU Time and Gray Failures](./26-cpu-and-gray-failure.md) |

## Every Builder Method

| Method | Purpose | Chapter |
|---|---|---|
| `new()` | Empty builder. `Default` is the same. | this chapter |
| `with_providers(f)` | Empty builder whose actors get the providers bundle `f` builds instead of `SimProviders`. | [The Provider Pattern](../part2-foundations/04-provider-pattern.md#bringing-your-own-bundle) |
| `workload(w)` | One instance, reused across seeds, `client_id` 0. | [Writing a Workload](./03-writing-workload.md#one-instance-or-many) |
| `workload_factory(f)` | One fresh instance per seed and per explored timeline. | [Multiverse Exploration](../part5-building-on-top/01-exploration.md) |
| `workloads(count, f)` | `WorkloadCount::Fixed(n)` or `Random(range)` fresh instances per seed, indexed. | [Writing a Workload](./03-writing-workload.md#one-instance-or-many) |
| `processes(count, f)` | One process group; `usize` or `RangeInclusive<usize>` drawn per seed. Panics on a duplicate group name. | [Process Groups](./27-process-groups.md) |
| `cluster(config, f)` | One group laid out over `LocalityConfig` failure domains; the topology sets the count. | [Process Groups](./27-process-groups.md#cluster-failure-domains) |
| `tags(dims)` | Round-robin tag dimensions over the last group. Returns `Result`. | [Process Groups](./27-process-groups.md#tags) |
| `link_latency(config)` | Distance-dependent latency between `.cluster()` domains. | [Process Groups](./27-process-groups.md#cluster-failure-domains) |
| `tcp_send_window_bytes(n)` | End-to-end byte window per stream direction, every seed. | [Network Faults](./10-network-faults.md#flow-control) |
| `accept_backlog_capacity(n)` | Maximum unaccepted connections per listener. Must be positive. | [Network Faults](./10-network-faults.md#flow-control) |
| `network_fault_mask(mask)` | Keep only these network families after per-seed sampling. Draws nothing. | [Swarm Testing](./28-swarm-testing.md) |
| `storage_fault_mask(mask)` | The storage twin of `network_fault_mask`. | [Swarm Testing](./28-swarm-testing.md) |
| `replicated_storage_faults(config)` | Spread storage damage over a group's failure domains so no record is damaged everywhere. | [Aiming Storage Faults](./33-aimed-faults.md) |
| `trace_level(level)` | The tracing floor for the subscriber and timeline. `INFO` by default. | [Reproducing](./21-reproducing.md#turning-up-the-logging) |
| `invariant(i)` | Register an `Invariant`, run after every step. | [Events and Invariants](./17-events-and-invariants.md) |
| `invariant_fn(name, f)` | Closure invariant over `&dyn TraceQuery`. | [Events and Invariants](./17-events-and-invariants.md) |
| `metrics_factory(f)` | One `MetricsSource` per node IP per seed, scraped into the report. | [Application Metrics](./25-metrics.md) |
| `metric(query)` | A `MetricQuery` evaluated after every successful seed. | [Application Metrics](./25-metrics.md) |
| `fault_factory(f)` | A fresh custom `FaultInjector` per timeline. Requires `chaos_duration`. | [Custom Fault Injectors](./31-custom-faults.md) |
| `chaos_duration(d)` | The window in which new faults may be injected. | [Chaos in Moonpool](./07-chaos.md) |
| `enable_chaos(surfaces)` | Enable `Chaos` surfaces with a `ChaosMode` each. | this chapter, [Swarm Testing](./28-swarm-testing.md) |
| `swarm_operations()` | Per-seed subset of each workload's operation alphabet via `swarm_op_enabled`. | [Designing Workloads](./19-designing-workloads.md#swarm-the-alphabet) |
| `set_iterations(n)` | `IterationControl::FixedCount(n)`. | this chapter |
| `until_coverage_stable(plateau, cap)` | `IterationControl::UntilCoverageStable`. The default is `(10, 1000)`. | [Running and Observing](./05-running.md#stop-conditions) |
| `set_debug_seeds(seeds)` | Use these seeds first. | [Reproducing](./21-reproducing.md) |
| `replay_timeline(seed, recipe)` | One iteration of `seed` with an exploration `BugRecipe`'s RNG breakpoints. | [Exemplars and Continuations](../part5-building-on-top/05-exemplars-continuations.md) |
| `run_time_budget(d)` | Virtual-time budget per workload phase. One simulated hour by default. | [Running and Observing](./05-running.md#stall-detection) |
| `check_determinism()` | Run every seed twice and fail it when the draw sequences differ. | [Reproducing](./21-reproducing.md#when-the-bug-disappears) |
| `enable_exploration(config)` | Frontier exploration from novel assertion outcomes. Needs the `exploration` feature. | [Multiverse Exploration](../part5-building-on-top/01-exploration.md) |
| `run()` | Run the campaign. `Result<SimulationReport, SimulationError>`. | [Running and Observing](./05-running.md) |

Three groups of methods consume no randomness: the masks, the deployment-shape settings (`tcp_send_window_bytes`, `accept_backlog_capacity`, `link_latency`) and `trace_level`. Adding them to a campaign changes no seed's schedule. Every `Chaos` surface, every ranged count and `swarm_operations` draw from the simulation stream in a fixed order, so adding one shifts every seed. That is intended, and [Seed-Driven Reproducibility](../part2-foundations/03-seeds.md) explains the one-stream rule behind it.

## Putting It All Together

```rust
let report = SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
    .tags(&[("role", &["primary", "replica"])])?
    .workloads(WorkloadCount::Random(1..4), |_| Box::new(KvWorkload::new(200)))
    .invariant(SingleLeader { cursor: Cell::new(0) })
    .chaos_duration(Duration::from_secs(30))
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::BuggifyKnobs,
    ])
    .swarm_operations()
    .until_coverage_stable(10, 2_000)
    .run()?;
```

The builder creates the simulated world, assigns IPs, seeds the stream, runs the orchestration loop, collects assertions and metrics, and produces the report. [Running and Observing](./05-running.md) reads that report.
