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
| `tags(dimensions)` | `&[(&str, &[&str])]` | Attach round-robin tag distribution to processes |
| `attrition(config)` | `Attrition` | Enable automatic process reboots during chaos phase |
| `invariant(i)` | `impl Invariant` | Add an invariant checked after every simulation event |
| `invariant_fn(name, f)` | `String`, closure | Add a closure-based invariant |
| `fault(f)` | `impl FaultInjector` | Add a custom fault injector for the chaos phase |
| `phases(config)` | `PhaseConfig` | Set chaos/recovery phase durations |
| `set_iterations(n)` | `usize` | Run exactly N iterations (default: 1) |
| `set_iteration_control(ctrl)` | `IterationControl` | Set the iteration control strategy |
| `set_time_limit(dur)` | `Duration` | Run for a wall-clock time duration |
| `set_debug_seeds(seeds)` | `Vec<u64>` | Use specific seeds for deterministic debugging |
| `random_network()` | -- | Enable randomized `NetworkConfiguration` per iteration |
| `enable_exploration(config)` | `ExplorationConfig` | Enable fork-based multiverse exploration |
| `replay_recipe(recipe)` | `BugRecipe` | Replay a specific bug recipe |
| `run()` | -- | Execute the simulation, returns `SimulationReport` |

### Default state

A freshly created `SimulationBuilder::new()` has:

- **iteration_control**: `IterationControl::FixedCount(1)`
- **use_random_config**: `false` (uses `NetworkConfiguration::default()`)
- **exploration**: disabled
- **seeds**: empty (auto-generated)
- No workloads, processes, invariants, or fault injectors

## IterationControl

Controls how many iterations a simulation runs.

| Variant | Type | Description |
|---------|------|-------------|
| `FixedCount(n)` | `usize` | Run exactly `n` iterations |
| `TimeLimit(duration)` | `Duration` | Run for a wall-clock time duration |

**Note**: The `UntilAllSometimesReached(N)` pattern mentioned in CLAUDE.md is implemented at the test level by checking assertion coverage, not as a variant of `IterationControl`.

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

## PhaseConfig

Two-phase chaos/recovery configuration. Required for attrition and fault injectors to activate.

| Field | Type | Description |
|-------|------|-------------|
| `chaos_duration` | `Duration` | Duration of the chaos phase (faults + workloads run concurrently) |
| `recovery_duration` | `Duration` | Duration of the recovery phase (faults stopped, workloads continue) |

## Attrition

Built-in configuration for automatic process reboots during the chaos phase. Requires `.phases()` to be set.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_dead` | `usize` | -- | Maximum number of simultaneously dead processes |
| `prob_graceful` | `f64` | -- | Weight for graceful reboots (signal + grace period) |
| `prob_crash` | `f64` | -- | Weight for crash reboots (immediate kill) |
| `prob_wipe` | `f64` | -- | Weight for crash + storage wipe reboots |
| `recovery_delay_ms` | `Option<Range<usize>>` | `1000..10000` | Delay before restarting a killed process (ms) |
| `grace_period_ms` | `Option<Range<usize>>` | `2000..5000` | Time allowed for graceful shutdown before force-kill (ms) |

The `prob_*` fields are **weights**, not probabilities. They are normalized internally and do not need to sum to 1.0.

### RebootKind

The type of reboot chosen based on attrition probabilities:

| Variant | Behavior |
|---------|----------|
| `Graceful` | Signal shutdown token, wait grace period, drain send buffers, then restart |
| `Crash` | Immediate task cancel, all connections abort, no buffer drain |
| `CrashAndWipe` | Same as Crash plus storage wipe (storage scoping is future work) |

## NetworkConfiguration

Top-level network simulation parameters.

| Field | Type | Default |
|-------|------|---------|
| `bind_latency` | `Range<Duration>` | 50us..150us |
| `accept_latency` | `Range<Duration>` | 1ms..6ms |
| `connect_latency` | `Range<Duration>` | 1ms..11ms |
| `read_latency` | `Range<Duration>` | 10us..60us |
| `write_latency` | `Range<Duration>` | 100us..600us |
| `chaos` | `ChaosConfiguration` | See below |

### Constructor variants

| Constructor | Description |
|-------------|-------------|
| `NetworkConfiguration::default()` | Standard defaults with chaos enabled |
| `NetworkConfiguration::random_for_seed()` | Randomized per seed for chaos testing |
| `NetworkConfiguration::fast_local()` | Minimal latencies, all chaos disabled |

## ChaosConfiguration

All fault injection settings for the simulated network. Part of `NetworkConfiguration`.

### Clogging

| Field | Type | Default |
|-------|------|---------|
| `clog_probability` | `f64` | 0.0 |
| `clog_duration` | `Range<Duration>` | 100ms..300ms |

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

### Latency Distribution

| Field | Type | Default |
|-------|------|---------|
| `latency_distribution` | `LatencyDistribution` | `Uniform` |
| `slow_latency_probability` | `f64` | 0.001 (0.1%) |
| `slow_latency_multiplier` | `f64` | 10.0 |

**LatencyDistribution** variants: `Uniform`, `Bimodal` (99.9% fast, 0.1% slow).

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
| `HalfCores` | Half of available cores (rounded up, min 1) |
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
