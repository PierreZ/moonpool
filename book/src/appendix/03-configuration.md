# Configuration Reference

<!-- toc -->

This chapter documents every configuration type in Moonpool with its fields, types, and default values. All values are sourced directly from the codebase.

## SimulationBuilder

The builder pattern for configuring and running simulation experiments. Created via `SimulationBuilder::new()`.

| Method | Parameters | Description |
|--------|-----------|-------------|
| `workload(w)` | `impl Workload` | Add a single workload instance, reused across iterations |
| `workload_with_client_id(cid, w)` | `ClientId`, `impl Workload` | Single workload with custom client ID strategy |
| `workloads(count, factory)` | `WorkloadCount`, `Fn(usize) -> Box<dyn Workload>` | Add factory-created workload instances |
| `workloads_with_client_id(count, cid, factory)` | `WorkloadCount`, `ClientId`, factory | Factory workloads with custom client IDs |
| `processes(count, factory)` | `impl Into<ProcessCount>`, `Fn() -> Box<dyn Process>` | Add server processes (system under test) |
| `cluster(config, factory)` | `LocalityConfig`, `Fn() -> Box<dyn Process>` | Add processes laid out across a datacenter/zone/machine topology (replaces `processes`) |
| `tags(dimensions)` | `&[(&str, &[&str])]` | Attach round-robin tag distribution to processes |
| `attrition(config)` | `Attrition` | Enable automatic process reboots during chaos phase |
| `invariant(i)` | `impl Invariant` | Add an invariant checked after every simulation event |
| `invariant_fn(name, f)` | `String`, closure | Add a closure-based invariant |
| `fault(f)` | `impl FaultInjector` | Add a custom fault injector for the chaos phase |
| `chaos_duration(dur)` | `Duration` | Set the chaos phase duration (faults run concurrently with workloads) |
| `set_iterations(n)` | `usize` | Run exactly N iterations (default: 1) |
| `set_iteration_control(ctrl)` | `IterationControl` | Set the iteration control strategy |
| `set_time_limit(dur)` | `Duration` | Run for a wall-clock time duration |
| `set_debug_seeds(seeds)` | `Vec<u64>` | Use specific seeds for deterministic debugging |
| `enable_chaos(surfaces)` | `impl IntoIterator<Item = Chaos>` | Enable network/storage/attrition chaos per seed, each in a `ChaosMode` (`Random` or `Swarm`) |
| `swarm_operations()` | -- | Enable per-seed swarm of each workload's operation alphabet |
| `enable_exploration(config)` | `ExplorationConfig` | Enable fork-based multiverse exploration |
| `replay_recipe(recipe)` | `BugRecipe` | Replay a specific bug recipe |
| `run()` | -- | Execute the simulation, returns `SimulationReport` |

### Default state

A freshly created `SimulationBuilder::new()` has:

- **iteration_control**: `IterationControl::UntilCoverageStable { plateau_seeds: 10, max_iterations: 1000 }`
- **chaos**: all surfaces off (network/storage use `default()`, no attrition)
- **swarm_operations**: `false` (workloads see the full operation alphabet)
- **exploration**: disabled
- **seeds**: empty (auto-generated)
- No workloads, processes, invariants, or fault injectors

## IterationControl

Controls how many iterations a simulation runs.

| Variant | Type | Description |
|---------|------|-------------|
| `UntilCoverageStable { plateau_seeds, max_iterations }` | `usize`, `usize` | **Default.** Stop once every observed `assert_sometimes!` / `assert_reachable!` has fired AND code coverage has not grown for `plateau_seeds` consecutive seeds. `max_iterations` is a safety cap. |
| `FixedCount(n)` | `usize` | Run exactly `n` iterations |
| `TimeLimit(duration)` | `Duration` | Run for a wall-clock time duration |

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

## ClientId

Strategy for assigning client IDs to workload instances.

| Variant | Type | Description |
|---------|------|-------------|
| `Fixed(base)` | `usize` | Sequential IDs starting from `base`: instance 0 gets `base`, instance 1 gets `base + 1`, etc. |
| `RandomRange(range)` | `Range<usize>` | Random ID drawn from `[start..end)` per instance (not guaranteed unique) |

**Default**: `Fixed(0)` (sequential starting from 0, matching FoundationDB's `WorkloadContext.clientId`).

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

The `prob_*` fields are **weights**, not probabilities. They are normalized internally and do not need to sum to 1.0.

### AttritionScope

Which failure domain a reboot kills. `PerMachine` and `PerZone` require a [`.cluster()`](../part3-building/09-attrition.md#failure-domains-correlated-reboots) topology; without locality they are a no-op.

| Variant | Behavior |
|---------|----------|
| `PerProcess` | Reboot one random process at a time (the default) |
| `PerMachine` | Reboot every process on a random machine together, atomically against `max_dead` |
| `PerZone` | Reboot every process in a random zone together, atomically against `max_dead` |

### RebootKind

The type of reboot chosen based on attrition probabilities:

| Variant | Behavior |
|---------|----------|
| `Graceful` | Signal shutdown token, wait grace period, drain send buffers, then restart |
| `Crash` | Immediate task cancel, all connections abort, no buffer drain |
| `CrashAndWipe` | Same as Crash plus immediate storage wipe for the process (scoped by IP) |

## NetworkConfiguration

Top-level network simulation parameters.

| Field | Type | Default |
|-------|------|---------|
| `bind_latency` | `LatencyDistribution` | `Uniform` 50us..150us |
| `accept_latency` | `LatencyDistribution` | `Uniform` 1ms..6ms |
| `connect_latency` | `LatencyDistribution` | `Uniform` 1ms..11ms |
| `read_latency` | `LatencyDistribution` | `Uniform` 10us..60us |
| `write_latency` | `LatencyDistribution` | `Uniform` 100us..600us |
| `chaos` | `ChaosConfiguration` | See below |

### Constructor variants

| Constructor | Description |
|-------------|-------------|
| `NetworkConfiguration::default()` | Standard defaults with chaos enabled |
| `NetworkConfiguration::random_for_seed()` | Randomized per seed for chaos testing |
| `NetworkConfiguration::fast_local()` | Minimal latencies, all chaos disabled |

### Latency distribution

Each per-operation latency field above is a `LatencyDistribution`, not a plain range. This lets a simulation exercise the heavy P99 tail where timeout cascades and retry storms live. Every variant samples deterministically through the simulation RNG.

| Variant | Shape | Models |
|---------|-------|--------|
| `Uniform { start, end }` | Flat over `[start, end)`, the default with unchanged behavior | Baseline jitter |
| `Exponential { min, mean }` | `min + mean * (-ln u)`, a long right tail | Slow disks, GC pauses (TigerBeetle) |
| `Bimodal { fast_range, slow_range, slow_probability }` | Fast cluster with a rare slow tail | Cross-datacenter hops, GC spikes (FoundationDB) |

`default()` and `fast_local()` keep every field `Uniform`, so behavior is unchanged unless you opt in. `random_for_seed()` mixes all three shapes per field for chaos seeds. The same `LatencyDistribution` type configures storage `read_latency`, `write_latency`, and `sync_latency`.

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

**PartitionStrategy** variants: `Random`, `UniformSize`, `IsolateSingle`.

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

## PeerConfig

Configuration for peer behavior and automatic reconnection. Part of moonpool-transport.

| Field | Type | Default |
|-------|------|---------|
| `initial_reconnect_delay` | `Duration` | 100ms |
| `max_reconnect_delay` | `Duration` | 30s |
| `max_queue_size` | `usize` | 1000 |
| `connection_timeout` | `Duration` | 5s |
| `max_connection_failures` | `Option<u32>` | `None` (unlimited) |
| `monitor` | `Option<MonitorConfig>` | `Some(MonitorConfig::default())` |

### Constructor variants

| Constructor | `initial_reconnect_delay` | `max_reconnect_delay` | `max_queue_size` | `connection_timeout` | `max_connection_failures` |
|-------------|---------------------------|----------------------|------------------|---------------------|--------------------------|
| `PeerConfig::default()` | 100ms | 30s | 1000 | 5s | None |
| `PeerConfig::local_network()` | 10ms | 1s | 100 | 500ms | Some(10) |
| `PeerConfig::wan_network()` | 500ms | 60s | 5000 | 30s | None |

## MonitorConfig

Ping-based connection health monitoring for peers. Follows FoundationDB's `connectionMonitor` pattern.

| Field | Type | Default |
|-------|------|---------|
| `ping_interval` | `Duration` | 1s |
| `ping_timeout` | `Duration` | 2s |
| `max_tolerated_timeouts` | `u32` | 3 |

### Constructor variants

| Constructor | `ping_interval` | `ping_timeout` | `max_tolerated_timeouts` |
|-------------|-----------------|----------------|--------------------------|
| `MonitorConfig::default()` | 1s | 2s | 3 |
| `MonitorConfig::local_network()` | 500ms | 1s | 2 |
| `MonitorConfig::wan_network()` | 5s | 10s | 5 |

## ExplorationConfig

Configuration for fork-based multiverse exploration. Passed to `SimulationBuilder::enable_exploration()`.

| Field | Type | Description |
|-------|------|-------------|
| `max_depth` | `u32` | Maximum fork depth (0 = no forking) |
| `timelines_per_split` | `u32` | Children per splitpoint in fixed-count mode |
| `global_energy` | `i64` | Total number of fork operations allowed |
| `adaptive` | `Option<AdaptiveConfig>` | Adaptive forking config; `None` = fixed-count mode |
| `parallelism` | `Option<Parallelism>` | Multi-core exploration; `None` = sequential |

### Parallelism

Controls how many forked children run concurrently.

| Variant | Slot count |
|---------|-----------|
| `MaxCores` | All available CPU cores |
| `HalfCores` | Half of available cores (integer division, min 1) |
| `Cores(n)` | Exactly `n` concurrent children |
| `MaxCoresMinus(n)` | All cores minus `n` (min 1) |

## AdaptiveConfig

Configuration for coverage-yield-driven batch forking. Used when `ExplorationConfig::adaptive` is `Some`.

| Field | Type | Description |
|-------|------|-------------|
| `batch_size` | `u32` | Children to fork per batch before checking coverage yield |
| `min_timelines` | `u32` | Minimum total forks per mark (even if barren after first batch) |
| `max_timelines` | `u32` | Hard cap on total forks per mark |
| `per_mark_energy` | `i64` | Initial energy budget per assertion mark |
| `warm_min_timelines` | `Option<u32>` | Minimum timelines for warm starts (multi-seed); defaults to `batch_size` if `None` |

### How the 3-level energy system works

1. **Global energy** (`global_energy`): hard cap on total timelines across all marks. When this hits 0, all exploration stops.
2. **Per-mark energy** (`per_mark_energy`): initial budget for each assertion mark. When exhausted, the mark draws from the reallocation pool.
3. **Reallocation pool**: energy returned by barren marks (marks that stopped producing new coverage). Productive marks can draw from this pool to continue exploring.

A mark is considered **barren** when a batch of children produces no new coverage bits and the mark has already spawned at least `min_timelines` (or `warm_min_timelines` during a warm start). Barren marks return their remaining per-mark energy to the reallocation pool.
