# Guide for AI Agents

<!-- toc -->

This page is a dense map of moonpool for a coding agent that adds deterministic simulation to a system. Every name on it exists in the current source. When this page and the code disagree, the code wins: read the source and update this page with the change. The narrative chapters explain each mechanism; this page shows how to combine them.

## The Rules

1. Production code is generic over `P: Providers`. It calls `time.sleep`, `task.spawn_task`, `network.connect`, `storage.open`, `random.random_range`, `system.sample`. Never `tokio::spawn`, `tokio::time`, `tokio::select!`, `std::fs`, `std::net`, `thread_rng`, `Instant::now`.
2. The same code runs in production over `TokioProviders` and in simulation over `SimProviders`. No `Sim*` wrapper in application code.
3. `Process` is the server, rebuilt by its factory on every reboot. `Workload` is the test driver and survives reboots. Never merge them.
4. Facts go out as `tracing` events. Cross-process safety goes in an `Invariant`. Local checks go in `assert_always!`. Rare paths get `assert_sometimes!`. Dangerous moments get `buggify!` or `hint!`.
5. Every random decision is a counted draw on one seeded stream. `BTreeMap`, not `HashMap`. Futures are `Send + 'static`. No `unwrap()`.
6. Replay before diagnosis: `set_debug_seeds(vec![seed]).set_iterations(1)` or `replay_timeline(seed, recipe)`.
7. Never delete an assertion that catches a bug. Fix the bug.
8. Validate with `nix develop --command cargo fmt`, `cargo clippy`, `cargo nextest run`, and `mdbook build book/` when a public API changes.

## Crates and Features

| Crate | Depend on it for | Features |
|-------|------------------|----------|
| `moonpool` | The facade. `moonpool_core::*`, `moonpool_sim::*` under `sim`, modules `hyper`, `prometheus`, `rpc`. | `default = ["sim", "tokio"]`, `hyper`, `prometheus`, `rpc`, `rpc-derive`, `rpc-tls`, `rpc-tls-pki`, `rpc-jwt` |
| `moonpool-core` | Provider traits and core types. | `default = ["tokio-providers", "select"]`, granular `tokio-task/-time/-net/-fs/-random/-system`, `deterministic-select` |
| `moonpool-sim` | The runtime, builder, chaos, assertions, observability. | `default = ["tokio-providers", "exploration"]` |
| `moonpool-assertions`, `moonpool-buggify` | Probes and fault sites for libraries. Zero dependencies. | none |
| `moonpool-explorer` | Multiverse exploration (`fork`, `mmap`). | none |
| `moonpool-hyper`, `moonpool-rpc`, `moonpool-prometheus`, `moonpool-journal` | HTTP/2, typed RPC, metrics, a CLSTORE journal. | see the [Crate Map](./appendix/02-crate-map.md) |

Production: `moonpool = { default-features = false, features = ["tokio"] }`. Simulation harness: `moonpool-sim` in dev-dependencies or a separate binary crate. wasm: `moonpool-sim` with `default-features = false`.

## Providers

| Trait | Methods | Simulation type |
|-------|---------|-----------------|
| `TimeProvider` | `sleep(d)`, `now() -> Duration`, `timer()` (drifted), `timeout(d, fut)` | `SimTimeProvider` |
| `TaskProvider` | `spawn_task(name, fut) -> JoinHandle`, `yield_now()`; `Detach` | `SimTaskProvider` |
| `NetworkProvider` | `bind(addr)`, `connect(addr)`; `TcpStream: AsyncRead + AsyncWrite`; `TcpListenerTrait::{accept, local_addr}` | `SimNetworkProvider` |
| `RandomProvider` | `random::<T>()`, `random_range(r)`, `random_ratio()`, `random_bool(p)` | `SimRandomProvider` |
| `StorageProvider` | `open(path, OpenOptions)`, `exists`, `delete`, `rename`, `create_dir_all`, `list_dir`, `sync_dir` | `SimStorageProvider` |
| `StorageFile` | `read_at`, `write_at`, `write_at_durable` (durable on return, its range only), the zero-copy `read_at_aligned` / `write_at_aligned` / `write_at_durable_aligned` (an owned `AlignedBuf` moved in and handed back; Tokio moves it into the blocking task instead of copying through a bounce buffer; defaults to the borrowed form, which the sim keeps), `sync_all`, `sync_data`, `size`, `set_len`, `allocate` (reserve the space: `fallocate(2)` on Linux; the sim counts it against disk capacity), `constraints()`, `is_direct_io()` | simulated file image |
| `SystemProvider` | `sample(data_dir) -> SystemSample { at, process_cpu, cores, machine_busy, machine_total, run_loop_busy, disk: Option<DiskCounters>, space: Option<DiskSpace> }` | `SimSystemProvider` |
| `Providers` | `network()`, `time()`, `task()`, `random()`, `storage()`, `system()` | `SimProviders` |
| `Resolver` | `resolve(host_port)` | `ScriptedResolver` |
| `Local*` (non-`Send`) | `LocalStorageProvider`, `LocalStorageFile`, `LocalNetworkProvider`, `LocalTcpListener`, `LocalTimeProvider`, `LocalProviders` (no task slot): blanket impls from every `Send` trait. `LocalTaskProvider::spawn_local_task(name, fut)`: no blanket impl; `CompioTaskProvider`, `SimTaskProvider`. Compio backend (features `compio-*`, umbrella `compio-providers`): `CompioTaskProvider`, `CompioTimeProvider`, `CompioNetworkProvider`, `CompioStorageProvider` (direct I/O and `O_APPEND` on Linux, same open policy as Tokio), `CompioSystemProvider`, bundle `CompioProviders: LocalProviders`, `CompioResolver`. `ThreadProviders: LocalProviders` adds a local task slot (`local_task()`): `CompioProviders`, `SimProviders` | the same sim types |

`OpenOptions::direct_io(DirectIo::{Disabled, Optional, Required})`, `BlockFile<F>` for block arithmetic over one open file (owned `*_blocks_aligned` forms for zero copy), `AlignedBuf` (`zeroed`, `for_constraints`, `truncate`, `is_aligned_to`), `LayoutRegion { path, bytes, kind, stripe, shard }` to name byte ranges. `moonpool_sim::select!` replaces `tokio::select!` with the same grammar. `moonpool_sim::executor::spawn(name, fut)` is the sim-only raw spawn.

## Process, Workload, Context

```rust,ignore
#[async_trait]
impl Process for Node {
    fn name(&self) -> &str { "node" }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> { .. }
}

#[async_trait]
impl Workload for Driver {
    fn name(&self) -> &str { "driver" }
    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }  // default
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> { .. }
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }  // default, after chaos
}
```

`SimContext` methods:

| Group | Methods |
|-------|---------|
| Providers | `providers() -> &P`; `network()`, `time()`, `random()`, `storage()` when `P: Providers`; `task()` is always the runner's `SimTaskProvider` |
| Custom bundle | `SimContext<P = SimProviders>`, `Process<P>`, `Workload<P>`, `Deployment<P>`. `SimulationBuilder::with_providers(\|env: &ProviderEnv\| P)` builds `P` per process boot and per workload. `P: ProviderBundle` (`Clone + Send + Sync + 'static`, `Providers` not required). Build it over the sim: `SimProviders::new(env.world().clone(), env.ip()).with_task(env.task().clone())`. Spawn only through `env.task()` (the boot scope a kill cancels) |
| Local code | `run_local(name, \|ctx\| fut)`: run a `!Send` future as a local task of the process, return its answer |
| Identity | `my_ip()`, `client_id()`, `client_count()`, `peer(name)`, `peers()` |
| Topology | `topology()` → `WorkloadTopology { groups, ips_in_group, group_for, my_group, peer_by_name, all_process_ips, ips_tagged, tags_for, my_tags, my_locality, locality_for, peers_on_my_machine, ips_in_domain }` |
| Lifecycle | `shutdown()` (a `CancellationToken`), `crash_self(kind, Option<Duration>)`, `self_crash() -> SelfCrash` |
| Deployment | `deployment() -> &Deployment { spawn(group), spawn_in(group, DomainSpec), spawn_group(name, count, factory), retire(ip), version() }`; refuses from a process; topology queries read the live `Membership` |
| Shared state | `state()` → `StateHandle { publish(key, v), get::<T>(key), contains(key) }` |
| Observation | `observability()` (a `TraceQuery`), `disk_counters(ip)`, `system_samples(ip)`, `slowness(ip)` |
| Metrics | `metrics::<S>()`, `metrics_handle()` |

## SimulationBuilder

```rust,ignore
let report = SimulationBuilder::new()
    .processes(3, || Box::new(Node::default()))              // group 0 on 10.0.1.{1..3}
    .tags(&[("role", &["leader", "follower"])])?             // round-robin over the last group
    .workloads(WorkloadCount::Fixed(2), |i| Box::new(Driver::new(i)))
    .enable_chaos([
        Chaos::Network(ChaosMode::Swarm),
        Chaos::Storage(ChaosMode::Swarm),
        Chaos::Attrition { config: attrition, mode: ChaosMode::Swarm },
    ])
    .chaos_duration(Duration::from_secs(60))
    .invariant(NoSplitBrain::new())
    .run()?;
if report.is_failure() { report.eprint(); std::process::exit(1); }
```

| Method | Note |
|--------|------|
| `processes(count, factory)`, `cluster(LocalityConfig, factory)` | `count`: `usize` or `RangeInclusive<usize>`. One call per role. |
| `tags(&[(key, &[values])]) -> Result` | Over the last group. |
| `workload(w)`, `workload_factory(f)`, `workloads(WorkloadCount, f)` | Exploration and `check_determinism` need a factory. |
| `enable_chaos([Chaos..])` | `Network(mode)`, `Storage(mode)`, `Attrition { config, mode }`, `Outage { config, mode }`, `BuggifyKnobs`, `Cpu(mode)`, `GrayFailure { config, mode }`. |
| `chaos_duration(d)` | Required by `fault_factory`. After it: recovery mode. |
| `chaos_until_run_ends()` | Chaos ends when the last `run()` returns (`chaos_duration` is the cap); `check()` runs on live processes after the cutoff. `chaos_ended_at()` gives the cutoff. |
| `fault_factory(\|\| Box<dyn FaultInjector>)` | Custom injector per run. |
| `network_fault_mask(m)`, `storage_fault_mask(m)` | Remove families after the draw. |
| `replicated_storage_faults(ReplicatedFaults)` | Bound damage to tolerated domains. |
| `swarm_operations()` | Per-seed operation subsets; test with `swarm_op_enabled(op_id)`. |
| `link_latency(cfg)`, `tcp_send_window_bytes(n)`, `accept_backlog_capacity(n)` | Deployment shape. Defaults none, 64 KiB, 128. |
| `max_spawns(n)` | Cap on processes workloads spawn per run through `ctx.deployment()`. Default 16. |
| `invariant(i)`, `invariant_fn(name, \|q, t\| ..)`, `trace_level(level)` | Invariants run after every step. |
| `metrics_factory(\|ip\| Arc<S>)`, `metric(MetricQuery::select("m").label(k, v).rate().bucketize(d, Mean).reduce(Mean).named("x"))` | App metrics in the report. |
| `set_iterations(n)`, `until_coverage_stable(10, 1000)` (default), `set_debug_seeds(seeds)`, `run_time_budget(d)` (1 h) | Campaign control. |
| `enable_exploration(ExplorationConfig)`, `replay_timeline(seed, recipe)`, `check_determinism()` | Exploration, replay, canary. |

No builder method accepts a `NetworkConfiguration` or `StorageConfiguration`. Without `Chaos::Network` the network still has FoundationDB's always-on faults (random close, clock drift, buggified delay, probabilistic connect failure). Without `Chaos::Storage` storage has no faults.

## SimulationReport

| Field | Meaning |
|-------|---------|
| `iterations`, `successful_runs`, `failed_runs`, `seeds_used`, `seeds_failing` | Campaign outcome. `is_failure()` is true when a seed failed, an always-assertion was violated, or assertion slots were dropped. |
| `assertion_results`, `assertion_details`, `bucket_summaries`, `assertion_violations`, `coverage_violations`, `dropped_assertion_allocations` | Assertion accounting. |
| `saturation: Option<SaturationReport { signal, edges_covered, edges_total, sometimes_hit, sometimes_total, plateau_seeds }>`, `convergence_timeout` | How `UntilCoverageStable` stopped. |
| `exploration: Option<ExplorationReport { total_timelines, expansions, discoveries, bugs_found, bug_recipes, max_active_workers, sancov_edges_total, sancov_edges_covered, converged, per_seed_timelines }>` | Exploration. |
| `metrics`, `individual_metrics`, `app_metrics`, `metric_queries`, `run_id` | Timing and application metrics. |

`report.eprint()` prints it. `success_rate()`, `average_wall_time()`, `average_simulated_time()`.

## Assertions

| Macro | Role |
|-------|------|
| `assert_always!(cond, "msg" [, { "k" => v }])`, `assert_always_or_unreachable!` | Invariant. Violation fails the seed. |
| `assert_sometimes!(cond, "msg")`, `assert_reachable!("msg")`, `assert_unreachable!("msg")` | Coverage. First pass is a discovery. |
| `assert_always_{greater_than, greater_than_or_equal_to, less_than, less_than_or_equal_to}!(val, thresh, "msg")` | Bound. Watermark for reporting only. |
| `assert_sometimes_{..}!(val, thresh, "msg")` | Depth. Watermark improvements guide the explorer. |
| `assert_sometimes_all!("msg", [("a", a), ("b", b)])` | Facts that must coincide. Frontier advances guide. |
| `assert_sometimes_each!("msg", [("phase", p)], [("quality", q)])` | Per-identity coverage. At most 6 keys, 256 buckets. No contract. |
| `reachable!("msg")`, `sometimes!(cond, "msg")` | Probes from `moonpool-assertions` for libraries. |

Limits: 2048 sites, 256 buckets. Only sometimes-style assertions create discoveries. Full table: [Assertion Reference](./appendix/01-assertion-reference.md).

## Invariants and Traces

```rust,ignore
tracing::info!(target: "raft", term, leader = %my_ip, "leader_elected");   // emit, in process code

impl Invariant for NoSplitBrain {
    fn name(&self) -> &'static str { "no_split_brain" }
    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        for e in q.since("leader_elected", &self.cursor) {       // cursor: Cell<usize>
            let term = e.u64("term"); let leader = e.str("leader");
            assert_always!(.., "one leader per term");
        }
    }
    fn reset(&mut self) { .. }                                    // per seed
}
```

Capture rule: INFO and above, constant message, inside a process or workload task. `TraceEvent { seq, time_ms, source, target, level, name, fields }` with `u64`, `i64`, `f64`, `bool`, `str` accessors. `TraceQuery { len(name), since(name, &cursor), snapshot(name) }`. Sim faults appear as `"sim_fault"` events with a `kind` field. `init_sim_tracing(Level)` in a binary's `main`.

## Buggify and Hints

| Macro | Effect |
|-------|--------|
| `buggify!()` | 25% at an active site (sites are active on half the seeds). |
| `buggify_with_prob!(p)`, `buggify_fault_with_prob!(p)` | `p` at an active site. The fault form is silent in recovery mode. |
| `buggify_named!("label", p)` + `set_activation("label", bool)` | Named sites a harness can mask. |
| `buggify_pick!(p, n)`, `buggify_range!(p, range)` | `Option<usize>`, `Option<u64>`. |
| `buggify_knob!(default, lo..hi)` | A knob value in range on some seeds (`moonpool-sim`). |
| `hint!("label")`, `hint!("label", p)` | A crash moment. The attrition regime decides. `HintVeto::new(\|ip, label\| bool).publish(ctx.state())` budgets it, and a per-process regime's timed reboots too (label `TIMED_REBOOT_LABEL`). |
| `cpu_charge(Duration)` | Explicit CPU work under `Chaos::Cpu`. |

All are no-ops in production. `moonpool-journal` shows the pattern: four `hint!` moments per batched commit (one after a failed sync writes the batch again) (an ordered commit's two durable writes are named inside the simulated `write_at_durable`, "storage: durable write landed, not synced"), and two per roll onto a recycled spare segment.

## Chaos Configuration

| Type | Construction |
|------|--------------|
| `Attrition` | `{ max_dead, prob_graceful, prob_crash, prob_wipe, recovery_delay_ms: None (1–10 s), grace_period_ms: None (2–5 s), scope: AttritionScope::PerProcess, victims: AttritionVictims::Any, wipe_budget: None }`. `AttritionVictims::group("name")`, `::tagged("key", "value")`. `.wipe_budget(max_unhealed, WipeHeal::Signal \| WipeHeal::After(d))` makes wipes roll: a wiped process is unhealed until `WipeLedger::heal(ctx.state(), ip)` (or `d` after its restart), and a drawn wipe past `max_unhealed` unhealed in the pool becomes a crash. No extra draws. |
| `Outage` | `Outage::groups(["acceptors"])`: probability 1.0, start 0–10 s, down 1–10 s, no straggler. `OutageVeto::new(\|victims\| bool).publish(ctx.state())`. |
| `GrayFailure` | `GrayFailure::new(max_slow)`: window 0.3–1.0 of the chaos window, cpu 10–1000, disk 5–100, network 2–20, `GrayKinds::ALL`. |
| `CpuModel` | Drawn per seed under `Chaos::Cpu`; `SimWorld::set_cpu_model` by hand. |
| `NetworkFaultMask`, `StorageFaultMask` | `all()`, `none()`, `.with(f)`, `.without(f)`. `NetworkFault::{Clog, Partition, BitFlip, RandomClose, ConnectFailure, ClockDrift, BuggifiedDelay, PairLatency, BlackHole}`. `StorageFault::{Corruption, Eio, Misdirect, PhantomWrite, SyncFailure, ShortTransfer, DirEntryLoss, CrashDamage, Degradation, DiskFailure, DroppedMutationDiscard}`. A dropped write, sync or `set_len` future still lands at its scheduled time (as on Tokio and io_uring) unless `dropped_mutation_discard_probability` discards it whole. A dropped read is cancelled. |
| `ReplicatedFaults` | `ReplicatedFaults::new(DomainLevel::Machine).tolerance(1).group("acceptors")`. Patterns `Minority`, `Striped`, `Rolling`, `Spared`. |
| `FaultFocus` | `FaultFocus::new().background(0.0).layout(journal.regions().iter(), \|r\| if r.kind == Layout::RECORD { 1.0 } else { 0.0 })` (`Journal::regions()` returns `Vec<LayoutRegion>`). |
| `LocalityConfig` | `LocalityConfig::new(datacenters, zones, machines, processes)` with `cluster(..)`. |
| `ExplorationConfig` | `{ workers: 0, max_runs_per_seed: 512, branching_factor: 4, max_frontier: 1024, max_recipe_len: 64 }`. |

Every default and range: [Configuration Reference](./appendix/03-configuration.md). Every fault: [Fault Reference](./appendix/04-fault-reference.md).

## FaultContext (custom injectors)

```rust,ignore
#[async_trait]
impl FaultInjector for Partitioner {
    fn name(&self) -> &str { "partitioner" }
    async fn inject(&mut self, ctx: &FaultContext) {
        while !ctx.chaos_shutdown().is_cancelled() {
            if let Some(ip) = ctx.reboot_random(RebootKind::Crash).ok().flatten() { .. }
            ctx.time().sleep(Duration::from_secs(5)).await.ok();
        }
    }
}
```

| Group | Methods |
|-------|---------|
| Reboots | `reboot(ip, kind)`, `reboot_with_delays(..)`, `crash(ip)`, `crash_for(ip, down)`, `restart(ip)`, `reboot_random(kind)`, `reboot_tagged(..)`, `reboot_machine(..)`, `reboot_domain(..)`, `dead_count()`, `is_dead(ip)` |
| Network | `partition(a, b)`, `heal_partition(a, b)`, `is_partitioned(a, b)` |
| Storage | `durable_bytes(ip, path)`, `corrupt_bytes(..)`, `fail_file_with_eio(..)`, `clear_file_eio(..)`, `set_disk_capacity(ip, Option<DiskCapacity>)`, `disk_space(ip)` |
| Slowness | `set_slowness(ip, Slowness)`, `slowness(ip)`, `cpu_model()` |
| Topology | `process_ips()`, `group_registry()`, `ips_in_group(name)`, `machine_registry()` (live, owned copies), `membership()`, `membership_version()` |
| Misc | `state()`, `random()`, `time()`, `chaos_shutdown()` |

## Exploration

```rust,ignore
.workload_factory(|| Box::new(Driver::default()))
.enable_exploration(ExplorationConfig { workers: 0, max_runs_per_seed: 2400, branching_factor: 4, max_frontier: 512, max_recipe_len: 32 })
```

`workers: 0` is deterministic and fork-free: use it in tests. `workers > 0` only in a standalone binary run through `cargo xtask sim run <name>`. Bugs land in `report.exploration.bug_recipes` as `BugRecipe { seed, recipe }`; replay with `replay_timeline(seed, recipe)`. `format_timeline` / `parse_timeline` convert `"151@8837201 -> 80@1293847"`. Guidance comes from sometimes-style assertions only. See [Multiverse Exploration](./part5-building-on-top/01-exploration.md).

## Low-Level SimWorld

For provider tests without the runner: `SimWorld::new_with_seed(seed)`, `new_with_network_config(cfg)`, `set_storage_config`, `set_process_storage_config(ip, cfg)`, `storage_provider(ip)`, `network_provider(ip)`, `time_provider()`, `step()`, `has_pending_events()`, `run_until_empty()`, `take_faults()`, `partition_pair(from, to, d)`, `corrupt_file(path, sectors)`, `fail_file_with_eio(path, sectors, EioTarget)`, `simulate_crash_for_process(ip, close_files)`, `set_fault_focus`, `publish_layout`. Drive futures with `moonpool_sim::executor::Executor::new(seed).block_on(..)` and step the world on `Pending` (the `drive` helper in the root `AGENTS.md`).

## Staged Adoption

1. **Boundary.** Make the code generic over `P: Providers`. Replace every forbidden call ([Sim Compatibility Checklist](./appendix/06-sim-compatibility.md)). Run one seed with no chaos. Add `check_determinism()` and keep it green.
2. **Driver and oracle.** Write the `Workload` with a small reference model (`BTreeMap` of expected state, not a copy of the server). Reconcile ambiguous results: a timed-out write may have committed.
3. **Facts and invariants.** Emit `tracing` events for every protocol decision. Write cursor-based invariants for agreement, validity, persistence, prefix, client semantics.
4. **Guidance.** Add `assert_sometimes!` on every retry, timeout and recovery path. Add watermarks for depth. Keep bucket vocabularies coarse.
5. **Chaos.** Enable `Chaos::Network(Swarm)`, then `Storage(Swarm)`, then `Attrition`, then `Outage`, `Cpu`, `GrayFailure`. Add `hint!` moments in storage code. Add `buggify!` at dangerous choices. Mask what the harness cannot observe.
6. **Campaign.** Keep the default `UntilCoverageStable`. Register the binary in xtask ([Running Simulation Binaries with xtask](./appendix/08-xtask.md)). Add exploration when a bug needs a sequence of rare events.
7. **Debug.** Pin the seed with `set_debug_seeds`, `set_iterations(1)`, `trace_level(LevelFilter::ERROR)`. Read the event trace. Fix the root cause. Re-enable chaos.

## Code to Read First

| Topic | File |
|-------|------|
| Invariant from traces | `crates/moonpool-sim/tests/leader_election.rs` |
| Process groups, tags, topology | `crates/moonpool-sim/tests/process_groups.rs`, `crates/moonpool-sim-examples/src/topology.rs` |
| Spawn and retire mid-run | `crates/moonpool-sim/tests/deployment.rs`, `crates/moonpool-sim-examples/src/operator.rs` |
| Swarm operations | `crates/moonpool-sim/tests/swarm_op_alphabet.rs` |
| Custom injectors, targeted faults | `crates/moonpool-sim/tests/scripted_faults.rs`, `targeted_faults.rs` |
| Hints, outages, self crash | `crates/moonpool-sim/tests/hint.rs`, `outage.rs`, `crash_self.rs` |
| Storage faults and crash model | `crates/moonpool-sim/tests/storage.rs`, `crates/moonpool-sim-examples/src/journal.rs` |
| CPU and gray failure | `crates/moonpool-sim/tests/cpu.rs` |
| Exploration | `crates/moonpool-sim-examples/src/maze.rs`, `dungeon.rs`, `crates/moonpool-sim/tests/exploration.rs` |
| Metrics | `crates/moonpool-sim/tests/metric_queries.rs`, `crates/moonpool-sim-examples/src/metrics_service.rs` |
| HTTP and gRPC | `crates/moonpool-sim-examples/src/axum_web.rs`, `tonic_grpc.rs` |
| Determinism | `crates/moonpool-sim/tests/determinism.rs`, `determinism_canary.rs` |
| RPC campaigns | `crates/moonpool-rpc-sim/src/` |
