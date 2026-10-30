# Configuration Reference

<!-- toc -->

Every value on this page is read from the current source. The code wins when the two disagree. Builder-level options come from `SimulationBuilder`. World-level options (network and storage profiles) are drawn per seed by the runner or set on `SimWorld` directly in low-level tests.

## SimulationBuilder

| Method | Default | Purpose |
|--------|---------|---------|
| `new()` | | Start a builder whose actors get `SimProviders`. |
| `with_providers(\|env: &ProviderEnv\| P)` | | Start a builder whose actors get the bundle `P` the factory builds, once per process boot and per workload. Processes and workloads then implement `Process<P>` and `Workload<P>`. |
| `processes(count, factory)` | | Register one server group. `count` is `usize` or `RangeInclusive<usize>` (`ProcessCount`). The g-th group lives on `10.0.{g+1}.x`. Duplicate group names panic. |
| `cluster(LocalityConfig, factory)` | | Register a group whose count comes from a sampled topology. The group may draw zero processes. |
| `tags(&[(key, &[values])]) -> Result<Self>` | | Round-robin tag dimensions over the last registered group. |
| `workload(instance)` | | One test driver instance. Not valid with exploration or `check_determinism`. |
| `workload_factory(\|\| Box<dyn Workload>)` | | A fresh driver per run. |
| `workloads(WorkloadCount, \|i\| Box<dyn Workload>)` | | `Fixed(n)` or `Random(range)` drivers per run. |
| `enable_chaos([Chaos, ..])` | no chaos | Enable chaos surfaces (table below). |
| `chaos_duration(Duration)` | none | The fault-generation window. Required by `fault_factory`. After it, `enter_recovery_mode` turns off every configuration-driven family and heals held partitions. |
| `chaos_until_run_ends()` | off | Chaos ends when the last workload's `run()` returns, `chaos_duration` its cap. An early `run()` stops no sibling, and `check()` runs on the live processes after the cutoff. |
| `fault_factory(\|\| Box<dyn FaultInjector>)` | | A custom injector per run. `run()` returns `InvalidConfiguration` without `chaos_duration`. |
| `network_fault_mask(NetworkFaultMask)` | `all()` | Remove network fault families after the per-seed draw. |
| `storage_fault_mask(StorageFaultMask)` | `all()` | Remove storage fault families after the per-seed draw. |
| `replicated_storage_faults(ReplicatedFaults)` | none | Bound storage damage to a tolerated set of failure domains. |
| `swarm_operations()` | off | Draw a per-seed subset of each workload's operation alphabet. |
| `link_latency(LinkLatencyConfig)` | none | Locality-aware link latency. |
| `tcp_send_window_bytes(usize)` | 64 KiB | Byte window per stream direction. |
| `accept_backlog_capacity(usize)` | 128 | Unaccepted connections per listener. Must be above 0. |
| `invariant(impl Invariant)` | | Run a cursor-based check after every step. |
| `invariant_fn(name, \|q, time_ms\| ..)` | | Closure form of `invariant`. |
| `trace_level(LevelFilter)` | `INFO` | Capture level for the timeline. |
| `metrics_factory(..)` | | One `MetricsSource` per node. |
| `metric(MetricQueryPlan)` | | A metric query reported per campaign. |
| `set_iterations(n)` | | `IterationControl::FixedCount(n)`. |
| `until_coverage_stable(plateau_seeds, max_iterations)` | `10, 1000` | `IterationControl::UntilCoverageStable`. This is the default. |
| `set_debug_seeds(Vec<u64>)` | | Pin seeds. They run first. |
| `run_time_budget(Duration)` | 1 simulated hour | A run past this budget is reported as a stall. |
| `replay_timeline(seed, recipe)` | | One seed, one iteration, the recipe installed as RNG breakpoints. |
| `enable_exploration(ExplorationConfig)` | off | Multiverse exploration. Needs factory workloads. Panics on a zero bound. |
| `check_determinism()` | off | Run every seed twice and fail on a diverging draw. Needs factory workloads. |
| `run() -> SimulationResult<SimulationReport>` | | Run the campaign. |

There is no builder method that takes a `NetworkConfiguration` or `StorageConfiguration`. The runner draws both per seed. Low-level tests set them on `SimWorld` with `new_with_network_config`, `set_network_config`, `set_storage_config` and `set_process_storage_config`.

### Enums

| Enum | Variants |
|------|----------|
| `IterationControl` | `FixedCount(usize)`, `UntilCoverageStable { plateau_seeds, max_iterations }` |
| `ProcessCount` | `Fixed(usize)`, `Range(RangeInclusive<usize>)` |
| `WorkloadCount` | `Fixed(usize)`, `Random(Range<usize>)` |
| `ChaosMode` | `Random` (every family on, rates drawn per seed), `Swarm` (a per-seed subset of families) |
| `Chaos` | `Network(ChaosMode)`, `Storage(ChaosMode)`, `Attrition { config, mode }`, `Outage { config, mode }`, `BuggifyKnobs`, `Cpu(ChaosMode)`, `GrayFailure { config, mode }` |

Without `Chaos::Network`, the network is **not** fault-free. It uses `NetworkConfiguration::default()`, whose `ChaosConfiguration::default()` keeps random closes, clock drift, buggified delays and probabilistic connect failures on, as FoundationDB does. Without `Chaos::Storage`, storage uses `StorageConfiguration::default()`, where every fault probability is 0.

### Draw Order

Every random decision is a counted draw on the one simulation stream. The runner draws in this order per seed:

1. Network profile and storage profile, **before** the reset that starts the RNG count. A replay breakpoint can never land on them.
2. The world reset.
3. Swarm operation masks (when enabled).
4. Attrition, outage, replicated faults, CPU model and gray-failure regimes.
5. The run itself.

Adding a draw anywhere shifts every later seed. That is intended. The same seed must still replay identically.

## Attrition

`Attrition` has no `Default`. Set every field:

| Field | Type | Default when `None` | Meaning |
|-------|------|---------------------|---------|
| `max_dead` | `usize` | | Simultaneously dead processes among this injector's victims. |
| `prob_graceful` | `f64` | | Weight for `RebootKind::Graceful`. |
| `prob_crash` | `f64` | | Weight for `RebootKind::Crash`. |
| `prob_wipe` | `f64` | | Weight for `RebootKind::CrashAndWipe`. |
| `recovery_delay_ms` | `Option<Range<usize>>` | `1000..10000` | Restart delay after a kill. |
| `grace_period_ms` | `Option<Range<usize>>` | `2000..5000` | Time between token cancel and force kill. |
| `scope` | `AttritionScope` | `PerProcess` | `PerProcess`, `PerMachine`, `PerZone`, `PerDatacenter`. |
| `victims` | `AttritionVictims` | `Any` | `Any`, `Group(String)` via `::group(name)`, `Tagged { key, value }` via `::tagged(key, value)`. |

The weights are relative. `Attrition::swarm_for_seed()` (used by `ChaosMode::Swarm`) draws 6 times: `max_dead` becomes 0 with probability 0.5, each weight is masked with probability 0.5, the recovery delay is scaled between 50% and 200%, and the scope is drawn among the scopes the topology supports.

## Outage

`Outage::groups(["acceptors", ..])` builds the default regime:

| Field | Default | Meaning |
|-------|---------|---------|
| `groups` | as given | Process groups that lose power together. |
| `probability` | 1.0 | Chance a seed has the outage at all. |
| `start` | `0s..10s` | When it strikes, from the start of the chaos window. A draw past the window means no outage. |
| `down` | `1s..10s` | How long each victim stays down. |
| `straggler` | `None` | One victim that stays down longer. Must start at or after `down`'s end. |

At most one outage per run. Under `Swarm` the outage is dropped on half the seeds and its straggler on half the rest. A workload publishes `OutageVeto::new(|victims: &[String]| bool)` through `ctx.state()` under `OUTAGE_VETO_KEY` (`"moonpool:outage-veto"`) to hold an outage. The simulator retries every `VETO_RETRY` (1 ms). The landed outage is published as `OutageLanded { at, victims, down, straggler }` under `OUTAGE_STATE_KEY` (`"moonpool.outage"`).

## Gray Failure

`GrayFailure::new(max_slow)`:

| Field | Default | Meaning |
|-------|---------|---------|
| `victims` | `AttritionVictims::Any` | Which processes may be slowed. |
| `max_slow` | as given | Machines slow at the same time. Use the tolerated loss `f`. |
| `window` | `0.3..=1.0` | Slow window as a fraction of the chaos window. |
| `cpu` | `10..=1000` | CPU slowdown factor range. |
| `disk` | `5..=100` | Disk latency factor range. |
| `network` | `2..=20` | Network latency factor range. |
| `kinds` | `GrayKinds::ALL` | `cpu`, `disk`, `network` flags. `Swarm` masks each per seed. |

`SimContext::slowness(ip)` and `FaultContext::slowness(ip)` return the current `Slowness { cpu, disk, network }`. `Slowness::HEALTHY` is all ones.

## CPU Model

`Chaos::Cpu(mode)` installs a `CpuModel` per seed:

| Field | Drawn from | Meaning |
|-------|-----------|---------|
| `poll_cost` | `POLL_COST_NS` = `500..5001` ns | Cost of one poll of one task. |
| `sent_byte_cost_ps` | `BYTE_COST_PS` = `500..5001` ps | Cost per byte sent. |
| `written_byte_cost_ps` | `BYTE_COST_PS` = `500..5001` ps | Cost per byte written to disk. |

`CpuModel::random_for_seed()` draws 3 times. `CpuModel::swarm_for_seed() -> Option<CpuModel>` draws one coin (0.5) and then the three costs. `cpu_charge(Duration)` adds explicit CPU time from application code. `SimWorld::set_cpu_model` installs a model by hand.

## Locality

`LocalityConfig::new(datacenters, zones_per_datacenter, machines_per_zone, processes_per_machine)`. Each argument is a `usize` or a `RangeInclusive<usize>` sampled per seed. `DomainLevel` is `Datacenter`, `Zone` or `Machine`. `LinkClass` is `SameMachine`, `SameZone`, `SameDatacenter` or `CrossDatacenter`.

`LinkLatencyConfig::default()`:

| Link class | Latency |
|------------|---------|
| `same_machine` | 10 µs to 50 µs |
| `same_zone` | 100 µs to 500 µs |
| `same_datacenter` | 500 µs to 2 ms |
| `cross_datacenter` | 20 ms to 80 ms |

## NetworkConfiguration

| Field | Default | `fast_local()` |
|-------|---------|----------------|
| `bind_latency` | Uniform 50 µs to 150 µs | 1 µs to 10 µs |
| `accept_latency` | Uniform 1 ms to 6 ms | 1 µs to 10 µs |
| `connect_latency` | Uniform 1 ms to 11 ms | 1 µs to 10 µs |
| `write_latency` | Uniform 100 µs to 600 µs | 1 µs to 10 µs |
| `link_latency` | `None` | `None` |
| `tcp_send_window_bytes` | 65536 (`DEFAULT_TCP_SEND_WINDOW_BYTES`) | same |
| `accept_backlog_capacity` | 128 (`DEFAULT_ACCEPT_BACKLOG_CAPACITY`) | same |
| `chaos` | `ChaosConfiguration::default()` | `ChaosConfiguration::disabled()` |

`NetworkConfiguration::random_for_seed()` and `swarm_for_seed()` draw the chaos block (below). `disable_fault_injection()` is what recovery mode calls.

`LatencyDistribution` variants: `Uniform { start, end }`, `Exponential { min, mean }`, `Bimodal { fast_range, slow_range, slow_probability }`. `sample_latency` and `sample_duration` draw from them.

## ChaosConfiguration (network)

| Field | `default()` | `random_for_seed()` range |
|-------|-------------|---------------------------|
| `clog_rate` | 0.0 | 0.05 to 1 clogs per simulated second |
| `clog_duration` | 100 ms to 300 ms | start 50 to 300 ms, end 100 to 500 ms |
| `partition_rate` | 0.0 | 0.05 to 0.5 partitions per simulated second |
| `partition_duration` | 200 ms to 2 s | start 100 ms to 1 s, end 500 ms to 3 s |
| `bit_flip_probability` | 0.0 | 0.00001 to 0.0002 |
| `bit_flip_min_bits` | 1 | 1 |
| `bit_flip_max_bits` | 32 | 32 |
| `bit_flip_cooldown` | 0 | 0 to 100 ms |
| `partial_write_max_bytes` | 1000 | 100 to 2000 |
| `partial_read_max_bytes` | 1000 | 100 to 2000 |
| `random_close_probability` | 0.00001 | 0.000001 to 0.0001 |
| `random_close_cooldown` | 5 s | 1 s to 10 s |
| `random_close_explicit_ratio` | 0.3 | 0.20 to 0.40 |
| `black_hole_probability` | 0.0 | 0.000001 to 0.0001 |
| `black_hole_cooldown` | 5 s | 1 s to 10 s |
| `clock_drift_enabled` | true | true |
| `clock_drift_max` | 100 ms | 50 ms to 150 ms |
| `buggified_delay_enabled` | true | true |
| `buggified_delay_max` | 100 ms | 50 ms to 150 ms |
| `buggified_delay_probability` | 0.25 | 0.20 to 0.30 |
| `connect_failure_mode` | `Probabilistic` | one of `Disabled`, `AlwaysFail`, `Probabilistic` |
| `connect_failure_probability` | 0.5 | 0.40 to 0.60 |
| `max_pair_latency` | 0 to 0 (off) | 0 to 0..100 ms |
| `partition_strategy` | `Random` | one of the 7 strategies |

`disabled()` zeroes every probability and turns every flag off. `swarm_for_seed()` draws `random_for_seed()` then 9 coins (0.5 each) that keep or drop clog, partition, bit flip, random close, connect failure, clock drift, buggified delay, pair latency and black hole. `apply_buggify_knobs()` (under `Chaos::BuggifyKnobs`) spikes clog to 2..5 per second, partition to 0.5..2 per second, random close to 0.01..0.1, black hole to 0.01..0.1, and bit flip to 0.001..0.01. It spikes a family only when it was already above 0, so a family the swarm turned off stays off.

`PartitionStrategy`: `Random`, `UniformSize`, `IsolateSingle`, `IsolateZone`, `IsolateDatacenter`, `AsymmetricSend`, `AsymmetricRecv`. `ConnectFailureMode`: `Disabled`, `AlwaysFail`, `Probabilistic`.

## StorageConfiguration

| Field | `default()` | `random_for_seed()` range | `fast_local()` |
|-------|-------------|---------------------------|----------------|
| `iops` | 25 000 | 10 000 to 100 000 | 1 000 000 |
| `bandwidth` (bytes/s) | 150 000 000 | 50 MB/s to 500 MB/s | 1 GB/s |
| `read_latency` | 50 µs to 200 µs | | 1 µs |
| `write_latency` | 100 µs to 500 µs | | 1 µs |
| `sync_latency` | 1 ms to 5 ms | | 1 µs |
| `read_corruption_probability` | 0.0 | 0 to 0.001 | 0 |
| `write_corruption_probability` | 0.0 | 0 to 0.001 | 0 |
| `read_eio_probability` | 0.0 | 0 to 0.001 | 0 |
| `write_eio_probability` | 0.0 | 0 to 0.001 | 0 |
| `clean_crash_probability` | 0.1 | | |
| `correlated_rollback_probability` | 0.25 | | |
| `correlated_rollback_max_run` | 8 | | |
| `crash_lost_probability` | 0.0 | 0 to 0.1 | 0 |
| `crash_latent_fault_probability` | 0.0 | 0 to 0.05 | 0 |
| `shorn_write_probability` | 0.0 | | 0 |
| `length_survives_crash_probability` | 0.5 | | |
| `garbage_fill_probability` | 1.0 | 0.5 | |
| `barrier_violation_probability` | 0.0 | | 0 |
| `misdirect_write_probability` | 0.0 | 0 to 0.0001 | 0 |
| `misdirect_read_probability` | 0.0 | 0 to 0.0001 | 0 |
| `phantom_write_probability` | 0.0 | 0 to 0.0002 | 0 |
| `sync_failure_probability` | 0.0 | 0 to 0.0005 | 0 |
| `direct_io_alignment` | 4096 | | |
| `direct_io_supported` | true | | |
| `unsynced_dir_entry_loss_probability` | 0.0 | 0 to 0.005 | 0 |
| `short_transfer_probability` | 0.0 | 0 to 0.002 | 0 |
| `disk_stall_probability` | 0.0 | 0 to 0.001 | 0 |
| `disk_stall_duration` | 0 | 50 ms to 500 ms | |
| `disk_throttle_probability` | 0.0 | 0 to 0.001 | 0 |
| `disk_throttle_duration` | 0 | 500 ms to 5 s | |
| `disk_throttle_iops_multiplier` | 1.0 | 2 to 20 | |
| `disk_throttle_bandwidth_multiplier` | 1.0 | 2 to 20 | |
| `disk_failure_probability` | 0.0 | 0 to 0.0001 | 0 |
| `dropped_mutation_discard_probability` | 0.0 | 0 to 0.49 | 0 |

`swarm_for_seed()` draws `random_for_seed()` then 15 coins over the families. `apply_buggify_knobs()` spikes iops to 100..5000, bandwidth to 1 MB/s..20 MB/s, sync failure to 0.05..0.2, disk stall to 0.1..0.5 with 100 ms to 500 ms episodes, throttle to 0.1..0.5 with 1 s to 5 s episodes, and disk failure to 0.001..0.01. `disable_fault_injection()` zeroes every family, including barrier violation. Sectors are `SECTOR_SIZE` = 512 bytes.

## Aiming Storage Faults

| Type | API |
|------|-----|
| `StorageEligibilityMask` | `Arc<dyn Fn(&str, u64) -> bool>` over `(path, sector)`. `SimWorld::set_storage_eligibility_mask`, `clear_storage_eligibility_mask`. |
| `FaultFocus` | `FaultFocus::new().background(w).spot(path, bytes, w).layout(regions, \|r\| w)`. `weight(path, sector)` is the heaviest spot or the background. `SimWorld::set_fault_focus`, `clear_fault_focus`, `SimStorageProvider::focus_faults`. |
| `LayoutRegion` | `{ path, bytes: Range<u64>, kind: &'static str, stripe: Option<u64>, shard: Option<String> }` from `moonpool_core`. Published with `SimWorld::publish_layout`. |
| `ReplicatedFaults` | `ReplicatedFaults::new(DomainLevel).tolerance(n).group("name")`. The pattern per seed is a `FaultPattern`: `Minority { domains }`, `Striped { domains, local }`, `Rolling { .. }`, `Spared`. |

## ExplorationConfig

| Field | Default | Meaning |
|-------|---------|---------|
| `workers` | 0 | Forked workers. 0 runs in-process and deterministic. |
| `max_runs_per_seed` | 512 | Ceiling on timelines per root seed. |
| `branching_factor` | 4 | Children per productive run. |
| `max_frontier` | 1024 | Queued jobs. |
| `max_recipe_len` | 64 | Depth in replay segments. |

`validate()` rejects a zero in any bound except `workers`. The worker wall-clock ceiling is `DEFAULT_WORKER_TIMEOUT` = 5 minutes (`Explorer::set_worker_timeout`).

## Buggify

| Item | Value |
|------|-------|
| `buggify_init(activation_prob)` | The runner calls `buggify_init(0.5)` per run: each site is active on half the seeds. |
| `buggify!()` | Fires with probability 0.25 at an active site. |
| `buggify_with_prob!(p)` | Fires with probability `p` at an active site. |
| `buggify_fault_with_prob!(p)` | Same, but silent in recovery mode. |
| `buggify_named!("label", p)` | Same, under a label that `set_activation(label, bool)` controls. |
| `buggify_pick!(p, n) -> Option<usize>` | An index below `n`, or `None`. |
| `buggify_range!(p, range) -> Option<u64>` | A value in `range`, or `None`. |
| `buggify_knob!(default, lo..hi)` | `moonpool-sim` only. The value in `lo..hi` with probability 0.25 at an active site, else `default`. |
| `hint!("label")`, `hint!("label", p)` | A fault moment. Default point probability 0.05. A no-op in production. |

See [Fault Reference](./04-fault-reference.md) for what each fault does.
