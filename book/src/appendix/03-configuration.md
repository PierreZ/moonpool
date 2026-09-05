# Configuration Reference

<!-- toc -->

This chapter documents every configuration type in Moonpool with its fields, types, and default values. All values are sourced directly from the codebase.

## SimulationBuilder

The builder pattern for configuring and running simulation experiments. Created via `SimulationBuilder::new()`.

| Method | Parameters | Description |
|--------|-----------|-------------|
| `workload(w)` | `impl Workload` | Add a single workload instance, reused across iterations |
| `workloads(count, factory)` | `WorkloadCount`, `Fn(usize) -> Box<dyn Workload>` | Add factory-created workload instances |
| `processes(count, factory)` | `impl Into<ProcessCount>`, `Fn() -> Box<dyn Process>` | Add one group of server processes (system under test); repeatable, one group per role, each on its own `10.0.{group}.x` range |
| `cluster(config, factory)` | `LocalityConfig`, `Fn() -> Box<dyn Process>` | Add one group of processes laid out across a datacenter/zone/machine topology (repeatable like `processes`) |
| `link_latency(config)` | `LinkLatencyConfig` | Give links a distance-dependent latency, resolved through the `cluster` topology |
| `network_fault_mask(mask)` | `NetworkFaultMask` | Suppress selected families after each Random/Swarm network profile is sampled; deterministic and exploration-safe |
| `tags(dimensions)` | `&[(&str, &[&str])]` | Attach round-robin tag distribution to processes |
| `invariant(i)` | `impl Invariant` | Add an invariant checked after every simulation event |
| `invariant_fn(name, f)` | `String`, closure | Add a closure-based invariant |
| `fault_factory(f)` | `Fn() -> Box<dyn FaultInjector>` | Add a custom fault injector for the chaos phase, rebuilt fresh for every root and explored timeline |
| `chaos_duration(dur)` | `Duration` | Bound the window in which new faults may be injected (see [Chaos duration and recovery mode](#chaos-duration-and-recovery-mode)) |
| `set_iterations(n)` | `usize` | Run exactly N iterations (default: 1) |
| `set_debug_seeds(seeds)` | `Vec<u64>` | Use specific seeds for deterministic debugging |
| `enable_chaos(surfaces)` | `impl IntoIterator<Item = Chaos>` | Enable network/storage/attrition chaos per seed, each in a `ChaosMode` (`Random` or `Swarm`) |
| `swarm_operations()` | -- | Enable per-seed swarm of each workload's operation alphabet |
| `check_determinism()` | -- | Run every seed twice and fail it if the replay's draw fingerprints differ (see [The Determinism Canary](../part2-foundations/03-seeds.md#the-determinism-canary)) |
| `enable_exploration(config)` | `ExplorationConfig` | Enable fork-based multiverse exploration |
| `replay_timeline(seed, recipe)` | `u64`, `Vec<(u64, u64)>` | Replay one explored timeline (a bug recipe) exactly |
| `run()` | -- | Execute the simulation, returns `SimulationReport` |

### Default state

A freshly created `SimulationBuilder::new()` has:

- **iteration_control**: `IterationControl::UntilCoverageStable { plateau_seeds: 10, max_iterations: 1000 }`
- **chaos**: all surfaces off (network/storage use `default()`, no attrition)
- **swarm_operations**: `false` (workloads see the full operation alphabet)
- **check_determinism**: `false` (each seed runs once)
- **exploration**: disabled
- **seeds**: empty (auto-generated)
- No workloads, processes, invariants, or fault injectors

## IterationControl

Controls how many iterations a simulation runs.

| Variant | Type | Description |
|---------|------|-------------|
| `UntilCoverageStable { plateau_seeds, max_iterations }` | `usize`, `usize` | **Default.** Stop once every observed `assert_sometimes!` / `assert_reachable!` has fired AND code coverage has not grown for `plateau_seeds` consecutive seeds. `max_iterations` is a safety cap. |
| `FixedCount(n)` | `usize` | Run exactly `n` iterations |

**Note**: `UntilCoverageStable` uses real LLVM sancov code coverage when the binary is instrumented (built via `cargo xtask sim run`) and falls back to assertion-slot coverage otherwise. The report names the signal it used.

## ProcessCount

Controls how many process instances to spawn per iteration.

| Variant | Type | Description |
|---------|------|-------------|
| `Fixed(n)` | `usize` | Spawn exactly `n` processes every iteration |
| `Range(range)` | `RangeInclusive<usize>` | Spawn a seeded random count from the inclusive range |

Accepts `usize` or `RangeInclusive<usize>` via `Into<ProcessCount>`.

## WorkloadCount

Controls how many workload instances to spawn per iteration.

| Variant | Type | Description |
|---------|------|-------------|
| `Fixed(n)` | `usize` | Spawn exactly `n` instances |
| `Random(range)` | `Range<usize>` | Spawn a seeded random count from the half-open range |

## Chaos duration and recovery mode

`.chaos_duration(dur)` bounds the period during which Moonpool may inject **new**
faults. When it expires the runner crosses the chaos → recovery boundary in one
step: `ctx.chaos_shutdown()` is cancelled so fault injectors wind down, and
`SimWorld::enter_recovery_mode()` switches off every configuration-driven fault
family and heals the partitions the simulator is holding.

| At the cutoff | |
|---|---|
| Stopped | Network: partitions, clogs, bit flips, spontaneous closes, black holes, connect failures, clock drift, buggified sleep delays, new per-pair latency degradation |
| Stopped | Storage: read/write/sync/crash faults, misdirected and phantom writes, new disk stall and throttle episodes, new disk failures |
| Stopped | Block devices: EIO, read corruption, misdirected and phantom writes, persist failures, barrier violations |
| Stopped | Fault injectors, including built-in attrition |
| Healed | Every partition in force — directed pair cuts and asymmetric send-side / receive-side blocks alike |
| Preserved | Corrupted sectors, lost/misdirected/phantom writes already applied, connections already closed or black-holed, processes already killed, a disk that already failed (and the operations it parked), application state, the fixed extra latency a slow link already sampled |
| Left to expire | Finite effects already started: disk stall and throttle episodes, clogs, packets already scheduled with a delay |

The persistent consequences of faults already injected remain part of the
simulated state. **The cluster is not healthy at the cutoff** — only the
environment stops generating new faults. Recovering from the damage is the
simulated system's job.

The recovery window is the quiet tail between the cutoff and workload
completion, while the processes are still alive. The settle phase that follows
is *not* a recovery window: processes are aborted before it, so it only drains
the events left in the scheduler.

## Attrition

Built-in configuration for automatic process reboots during the chaos phase. Requires `.chaos_duration()` to be set.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_dead` | `usize` | -- | Maximum number of simultaneously dead processes |
| `prob_graceful` | `f64` | -- | Weight for graceful reboots (signal + grace period) |
| `prob_crash` | `f64` | -- | Weight for crash reboots (immediate kill) |
| `prob_wipe` | `f64` | -- | Weight for crash + storage wipe reboots |
| `recovery_delay_ms` | `Option<Range<usize>>` | `1000..10000` | Delay before restarting a killed process (ms) |
| `grace_period_ms` | `Option<Range<usize>>` | `2000..5000` | Time allowed for graceful shutdown before force-kill (ms) |
| `scope` | `AttritionScope` | `PerProcess` | Failure domain each reboot targets (see below) |
| `victims` | `AttritionVictims` | `Any` | Eligible victims: `Any`, `group(name)`, or `tagged(key, value)`; scopes the draw and `max_dead` to that pool |

The `prob_*` fields are **weights**, not probabilities. They are normalized internally and do not need to sum to 1.0.

In `ChaosMode::Swarm`, the recovery range is scaled per seed to 50%-200% of
its configured values. Clustered campaigns also swarm among topology-backed
scopes whose groups fit `max_dead`. Both decisions are draws on the simulation
stream, taken once at build time.

### AttritionScope

Which failure domain a reboot kills. `PerMachine`, `PerZone`, and `PerDatacenter` require a [`.cluster()`](../part3-building/09-attrition.md#failure-domains-correlated-reboots) topology; without locality they are a no-op.

| Variant | Behavior |
|---------|----------|
| `PerProcess` | Reboot one random process at a time (the default) |
| `PerMachine` | Reboot every process on a random machine together, atomically against `max_dead` |
| `PerZone` | Reboot every process in a random zone together, atomically against `max_dead` |
| `PerDatacenter` | Reboot every process in a random datacenter together, atomically against `max_dead` |

### RebootKind

The type of reboot chosen based on attrition probabilities:

| Variant | Behavior |
|---------|----------|
| `Graceful` | Signal shutdown token, wait grace period, drain send buffers, then restart |
| `Crash` | Immediate task cancel, all connections abort, no buffer drain |
| `CrashAndWipe` | Same as Crash plus immediate storage wipe for the process (scoped by IP) |

## NetworkConfiguration

Top-level network simulation parameters.

Bind, connect, and accept use their latency fields as genuinely delayed
operations. Each call remains pending until its targeted scheduler completion
fires. Established reads wait on buffered byte delivery or a network waker.
Write and link latency apply to ordered byte delivery after the stream accepts
available send-buffer capacity.

| Field | Type | Default |
|-------|------|---------|
| `bind_latency` | `LatencyDistribution` | `Uniform` 50us..150us |
| `accept_latency` | `LatencyDistribution` | `Uniform` 1ms..6ms |
| `connect_latency` | `LatencyDistribution` | `Uniform` 1ms..11ms |
| `write_latency` | `LatencyDistribution` | `Uniform` 100us..600us |
| `link_latency` | `Option<LinkLatencyConfig>` | `None` (distance-blind) |
| `chaos` | `ChaosConfiguration` | See below |

### Constructor variants

| Constructor | Description |
|-------------|-------------|
| `NetworkConfiguration::default()` | Standard defaults with chaos enabled |
| `NetworkConfiguration::random_for_seed()` | Randomized per seed for chaos testing |
| `NetworkConfiguration::swarm_for_seed()` | Randomized per seed, then restricted to a per-seed subset of fault families |
| `NetworkConfiguration::fast_local()` | Minimal latencies, all chaos disabled |

### Network fault mask

`SimulationBuilder::network_fault_mask()` applies a typed allow-mask after the
per-seed Random or Swarm profile and buggify knob perturbations, but before the
`SimWorld` is created. The mask consumes no simulation or configuration RNG
draws, so it is compatible with frontier exploration and exact recipe replay.
The default is `NetworkFaultMask::all()`, which leaves existing builders
unchanged.

```rust
SimulationBuilder::new()
    .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
    .network_fault_mask(
        NetworkFaultMask::all().without(NetworkFault::BitFlip),
    )
    .enable_exploration(exploration_config)
```

The mask covers `Clog`, `Partition`, `BitFlip`, `RandomClose`,
`ConnectFailure`, `ClockDrift`, `BuggifiedDelay`, `PairLatency`, and
`BlackHole`. Removing a
family can only suppress a fault; it cannot enable a family the sampled profile
turned off. Partial reads and writes are TCP/buggify behavior rather than
independently sampled fault families and remain active.

### Latency distribution

Each per-operation latency field above is a `LatencyDistribution`, not a plain range. This lets a simulation exercise the heavy P99 tail where timeout cascades and retry storms live. Every variant samples deterministically through the simulation RNG. Reads do not add a separate delay: they wait for bytes whose delivery already includes write and link latency.

| Variant | Shape | Models |
|---------|-------|--------|
| `Uniform { start, end }` | Flat over `[start, end)`, the default with unchanged behavior | Baseline jitter |
| `Exponential { min, mean }` | `min + mean * (-ln u)`, a long right tail | Slow disks, GC pauses (TigerBeetle) |
| `Bimodal { fast_range, slow_range, slow_probability }` | Fast cluster with a rare slow tail | Cross-datacenter hops, GC spikes (FoundationDB) |

`default()` and `fast_local()` keep every field `Uniform`, so behavior is unchanged unless you opt in. `random_for_seed()` mixes all three shapes per field for chaos seeds. The same `LatencyDistribution` type configures storage `read_latency`, `write_latency`, and `sync_latency`.

To replace the hand-picked defaults with values measured on a real machine, see [Calibrating Against a Real Machine](./07-calibration.md).

Every sampled delay enters the global `Scheduler<Event>`. Same-time events keep
their insertion order through stable schedule IDs, and dropping a delayed
network future cancels its live schedule.

### Link latency (distance-based)

`link_latency` is realism, not chaos: it lives on `NetworkConfiguration` and is applied whatever the chaos mode. Each ordered IP pair is classified through the installed locality topology, samples its class distribution once at first contact, and keeps that value for the run. The result lands in the same per-pair budget as `max_pair_latency` and is summed with it. A pair where either endpoint has no locality gets nothing.

| Field | Type | Default |
|-------|------|---------|
| `same_machine` | `LatencyDistribution` | `Uniform` 10us..50us |
| `same_zone` | `LatencyDistribution` | `Uniform` 100us..500us |
| `same_datacenter` | `LatencyDistribution` | `Uniform` 500us..2ms |
| `cross_datacenter` | `LatencyDistribution` | `Uniform` 20ms..80ms |

## ChaosConfiguration

All fault injection settings for the simulated network. Part of `NetworkConfiguration`.

### Clogging

| Field | Type | Default |
|-------|------|---------|
| `clog_probability` | `f64` | 0.0 |
| `clog_duration` | `Range<Duration>` | 100ms..300ms |

### Per-Pair Permanent Latency

| Field | Type | Default |
|-------|------|---------|
| `max_pair_latency` | `Range<Duration>` | `ZERO..ZERO` (off) |

Each ordered IP pair samples one fixed latency from this range at first contact and adds it to every delivery on that pair for the whole run (FoundationDB's `SimClogging`). An all-zero range disables it. FDB's `MAX_CLOGGING_LATENCY * random01()` is the `ZERO..MAX` case.

### Network Partitions

| Field | Type | Default |
|-------|------|---------|
| `partition_probability` | `f64` | 0.0 |
| `partition_duration` | `Range<Duration>` | 200ms..2s |
| `partition_strategy` | `PartitionStrategy` | `Random` |

**PartitionStrategy** variants: `Random`, `UniformSize`, `IsolateSingle`, `IsolateZone`, `IsolateDatacenter`, `AsymmetricSend`, `AsymmetricRecv`. The two `Isolate{Zone,Datacenter}` arms need a [`.cluster()`](../part3-building/09-attrition.md#failure-domains-correlated-reboots) topology and degrade to `Random` selection without one. The two `Asymmetric*` arms cut a single node one way, using the same primitives as `partition_send_from()` / `partition_recv_to()`.

### Bit Flips

| Field | Type | Default |
|-------|------|---------|
| `bit_flip_probability` | `f64` | 0.0001 (0.01%) |
| `bit_flip_min_bits` | `u32` | 1 |
| `bit_flip_max_bits` | `u32` | 32 |
| `bit_flip_cooldown` | `Duration` | 0 |

### Partial Writes

| Field | Type | Default |
|-------|------|---------|
| `partial_write_max_bytes` | `usize` | 1000 |

### Partial Reads

| Field | Type | Default |
|-------|------|---------|
| `partial_read_max_bytes` | `usize` | 1000 |

### Random Connection Close

| Field | Type | Default |
|-------|------|---------|
| `random_close_probability` | `f64` | 0.00001 (0.001%) |
| `random_close_cooldown` | `Duration` | 5s |
| `random_close_explicit_ratio` | `f64` | 0.3 (30% explicit) |

### Black Hole

A direction that accepts every write and delivers nothing, for the rest of the
connection's life; see [Black Holes](../part3-building/10-network-faults.md#black-holes).

| Field | Type | Default |
|-------|------|---------|
| `black_hole_probability` | `f64` | 0.0 (off) |
| `black_hole_cooldown` | `Duration` | 5s |

### Clock Drift

| Field | Type | Default |
|-------|------|---------|
| `clock_drift_enabled` | `bool` | `true` |
| `clock_drift_max` | `Duration` | 100ms |

### Buggified Delay

| Field | Type | Default |
|-------|------|---------|
| `buggified_delay_enabled` | `bool` | `true` |
| `buggified_delay_max` | `Duration` | 100ms |
| `buggified_delay_probability` | `f64` | 0.25 (25%) |

Builder campaigns apply this fault only to sleeps scheduled inside
`.chaos_duration()`. Setup and the post-chaos quiet tail do not consume its RNG
draws or receive extra delay (recovery mode also clears the flag outright at the
cutoff). A directly constructed `SimWorld` has no campaign window and applies the
configured fault for its whole lifetime, unless you call
`SimWorld::enter_recovery_mode()` yourself.

### Connection Failures

| Field | Type | Default |
|-------|------|---------|
| `connect_failure_mode` | `ConnectFailureMode` | `Probabilistic` |
| `connect_failure_probability` | `f64` | 0.5 (50%) |

**ConnectFailureMode** variants: `Disabled`, `AlwaysFail`, `Probabilistic` (50% refused, 50% hang).

### Handshake Delay

| Field | Type | Default |
|-------|------|---------|
| `handshake_delay_enabled` | `bool` | `true` |
| `handshake_delay_max` | `Duration` | 10ms |

## ExplorationConfig

Configuration for frontier-based exploration. Passed to `SimulationBuilder::enable_exploration()`.

| Field | Type | Description |
|-------|------|-------------|
| `workers` | `usize` | Max concurrent worker processes; `0` = in-process (sequential, deterministic, fork-free) |
| `max_runs_per_seed` | `u64` | Total timelines (root + exploration runs) per root seed |
| `branching_factor` | `u32` | Children enqueued when a run makes a new discovery (one expansion per run) |
| `max_frontier` | `usize` | Cap on queued jobs |
| `max_recipe_len` | `usize` | Depth cap in replay segments |

Live processes are bounded by `1 + workers` regardless of exploration depth. `max_runs_per_seed` is a ceiling, not a quota: a seed whose root run discovers nothing globally new stops after a single timeline.
