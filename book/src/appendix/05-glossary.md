# Glossary

<!-- toc -->

Terms are listed alphabetically. Cross-references are in **bold**. Type and macro names are the ones in the current source.

**Always assertion** -- An assertion that must hold on every evaluation (`assert_always!`, `assert_always_or_unreachable!`, the four `assert_always_*` numeric macros). A violation is recorded, never a panic, and fails the **seed** in the report.

**Antithesis principle** -- Assertions never crash the program. Violations are recorded and reported, so the run continues and finds cascading failures. All 15 macros follow it.

**Attrition** -- The built-in reboot regime (`Attrition`, `Chaos::Attrition`). Picks victims by **scope** and **victims** filter, keeps at most `max_dead` down, and chooses graceful, crash or crash-and-wipe reboots by weight. Also the regime that decides whether a **hint** may reboot.

**Buggify** -- FoundationDB's `BUGGIFY` model in `moonpool-buggify`. A site is active on half the seeds (`buggify_init(0.5)`); an active `buggify!()` fires 25% of the time. `buggify_named!` adds a label that `set_activation` controls. Decisions are seeded, so they replay.

**Buggify knob** -- `buggify_knob!(default, lo..hi)` in `moonpool-sim`: a configuration value that takes a random value in range on some seeds. `Chaos::BuggifyKnobs` applies the same idea to the network and storage profiles.

**Chaos surface** -- One entry of `SimulationBuilder::enable_chaos`: `Chaos::Network`, `Storage`, `Attrition`, `Outage`, `BuggifyKnobs`, `Cpu` or `GrayFailure`. Each carries a **chaos mode**.

**Chaos mode** -- `ChaosMode::Random` (every family on, rates drawn per seed) or `ChaosMode::Swarm` (a per-seed subset of families). See **swarm testing**.

**Chaos window** -- The `chaos_duration` during which injectors generate faults. At its end the runner enters **recovery mode**. The consequences of earlier faults remain.

**CPU model** -- `CpuModel { poll_cost, sent_byte_cost_ps, written_byte_cost_ps }`. Under `Chaos::Cpu` every poll, sent byte and written byte costs simulated time on the process's one core. `cpu_charge(Duration)` adds explicit work.

**Crash model** -- At a crash, every sector written since the last sync resolves independently to a `CrashOutcome`: `KeptOld`, `KeptNew`, `Lost`, `LatentFault` or `Shorn`. The barrier is the sync. See **lost-synced-write oracle**.

**Determinism** -- The same **seed** produces the same execution. Every draw goes through one counted RNG stream, and all I/O is simulated.

**Determinism canary** -- `SimulationBuilder::check_determinism()`. Each seed runs twice; a diverging draw fails the seed with the `"determinism canary"` always-assertion and names the first diverging draw.

**Discovery** -- A globally new interesting state seen by the assertion accounting: a first pass, a new bucket, a watermark, frontier or quality improvement. Latched by compare-and-swap, so it is journaled once across every **timeline** and **seed**. The explorer anchors continuations at discoveries.

**Exemplar** -- A retained **recipe** plus the RNG call count of the discovery it reaches. The explorer keeps up to 3 per semantic state and rotates them.

**Explorer** -- The frontier controller in `moonpool-explorer`. Owns the **frontier**, exemplars, statistics and the bounded **worker** pool. Talks to the simulation only through the assertion discovery hook and an RNG call-count function.

**Fault focus** -- `FaultFocus`: weights per byte range (`spot`) or per `LayoutRegion` (`layout`) plus a background weight that steer the random storage families toward one structure. Installed with `SimWorld::set_fault_focus`.

**Fault injector** -- A custom chaos driver: `FaultInjector { name, inject(&FaultContext) }`, built fresh per run by `fault_factory`. `FaultContext` gives reboots, partitions, byte corruption, slowness and the shared **state handle**.

**Fault timeline** -- The `"sim_fault"` events (`SIM_FAULT_EVENT_NAME`) the runner merges into the **trace timeline**, with `source = "sim"` and a `kind` field.

**Frontier (assertion)** -- For `assert_sometimes_all!`: the largest number of named facts true at once. Advancing it is a discovery.

**Frontier (exploration)** -- The explorer's FIFO queue of jobs, bounded by `max_frontier`.

**Gray failure** -- A machine that stays alive and slow (`GrayFailure`, `Chaos::GrayFailure`). At most `max_slow` machines at once, each with CPU, disk and network multipliers for a window. Read with `SimContext::slowness(ip)`.

**Hint** -- `hint!("label")`: library code names a moment where a crash is interesting (entries written, not synced). A no-op in production. In simulation the runner may reboot the process there under the seed's **attrition** regime.

**HintVeto** -- `HintVeto::new(|ip, label| bool).publish(ctx.state())`: a workload's budget on which hints may strike. Stored under `HINT_VETO_KEY`.

**Invariant** -- A cross-process property checked after every simulation step. Reads the **trace timeline** through `TraceQuery` (`since`, `snapshot`, `len`) and reports with `assert_always!`. Registered with `invariant` or `invariant_fn`.

**Iteration control** -- `IterationControl::FixedCount(n)` or `UntilCoverageStable { plateau_seeds, max_iterations }` (the default, 10 and 1000).

**Journal** -- `moonpool-journal`: a CLSTORE write-ahead log over `StorageProvider`. Positions in any order, a crash told apart from corruption, damage named by position and `Id`, and `Journal::layout` / `Journal::regions` exposing `LayoutRegion`s for aimed faults.

**LayoutRegion** -- `moonpool_core::LayoutRegion { path, bytes, kind, stripe, shard }`: a named byte range of a file. Published with `SimWorld::publish_layout`, consumed by **fault focus** and **replicated fault patterns**.

**Locality** -- The datacenter, zone and machine of a process (`LocalityInfo`), sampled from `LocalityConfig` by `SimulationBuilder::cluster`. Drives `LinkLatencyConfig`, partition strategies and attrition scopes.

**Lost-synced-write oracle** -- A sector that a sync reported durable and that changes across a crash fails the run with `StorageFaultKind::LostSyncedWrite`, unless `barrier_violation_probability` arms that lie on purpose.

**Multiverse** -- The logical tree of all **timelines** explored from one root **seed**. A data structure of recipes, never of live processes.

**Outage** -- `Outage`, `Chaos::Outage`: every process of the named groups loses power at one instant, each back after its own delay, with an optional straggler. The landed outage is published as `OutageLanded`.

**OutageVeto** -- `OutageVeto::new(|victims| bool).publish(ctx.state())`: a workload holds an outage until it permits it. Stored under `OUTAGE_VETO_KEY`; retried every `VETO_RETRY`.

**Process** -- The system under test. A server node that boots, is killed and restarts through its factory with fresh memory. Its only persistence is simulated storage. Registered with `processes` or `cluster`. FoundationDB's `fdbd`.

**Process group** -- One `processes` or `cluster` registration. The g-th group lives on `10.0.{g+1}.x` and is queried with `ctx.topology().ips_in_group(name)`.

**Provider** -- A trait over a runtime service: `TimeProvider`, `TaskProvider`, `NetworkProvider`, `RandomProvider`, `StorageProvider`, `SystemProvider`, bundled by `Providers`. Tokio implementations run in production, `SimProviders` in simulation. Same code, two worlds.

**Recipe** -- The replay path to a **timeline**: `Vec<(rng_call_count, seed)>` breakpoints from the root seed. Printed as `151@8837201 -> 80@1293847`. Replayed with `replay_timeline`.

**Recovery mode** -- The state after the **chaos window**: `SimWorld::enter_recovery_mode` turns every configuration-driven fault family off and heals held partitions. `buggify_fault_with_prob!` and hints fall silent. The system must recover on its own.

**Replicated fault pattern** -- `ReplicatedFaults::new(level).tolerance(n).group(name)` draws a `FaultPattern` (`Minority`, `Striped`, `Rolling`, `Spared`) that bounds storage damage to what a correct replication layer can repair.

**Run time budget** -- `run_time_budget`, 1 simulated hour by default. A run past it is a stall.

**Scheduler** -- The global `Scheduler<Event>` that owns monotonic simulated time, same-time FIFO order and cancellation.

**Seed** -- A `u64` that determines a simulation's randomness. Pinned with `set_debug_seeds`.

**SelfCrash** -- `SimContext::self_crash()` and `crash_self(kind, delay)`: a process kills itself at an exact point of its own protocol, without a fault injector and without a draw.

**Shard** -- `moonpool_buggify::shard!(name, current = r1[, next = r2])`: the code declares a shard's `Replication { replicas, progress, durable }` (`Need` = `AtLeast` / `All` / `Any`). The run's shard book (`ctx.shards()`) keeps every view not yet superseded, and the **shard gate** downgrades any destructive fault that would break a need: a kill becomes a clean restart, a wipe a crash, a crash a `KillKeepCache`, an outage spares victims, a partition or disk failure is not made, damage is not placed. `ShardFaults` adds aim, edge and a liveness core. Inert in production, like `hint!`.

**Slowness** -- `Slowness { cpu, disk, network }` multipliers on one process, `HEALTHY` when all are 1. Set by **gray failure** or `FaultContext::set_slowness`.

**Sometimes assertion** -- An assertion that must be true at least once across the campaign (`assert_sometimes!`, `assert_reachable!`, `assert_sometimes_*` numeric, `assert_sometimes_all!`). A miss is a coverage violation. Its passes are discoveries.

**State handle** -- `StateHandle` from `ctx.state()`: a per-run shared map where workloads publish vetoes and injectors read workload state.

**Storage fault mask** / **Network fault mask** -- `StorageFaultMask`, `NetworkFaultMask`: allow-masks applied after the per-seed draw, consuming no randomness. Used to remove a family a harness cannot observe (a sync lie, a silent disk failure).

**Swarm testing** -- Per-seed subsets: `ChaosMode::Swarm` masks fault families, `swarm_operations` masks each workload's operation alphabet, `buggify_named!` plus `set_activation` masks named sites. One workload, many behaviours.

**Timeline** -- One complete simulation run: a root **seed** plus a **recipe**. The root timeline has an empty recipe.

**Topology** -- `WorkloadTopology` from `ctx.topology()`: groups, IPs, tags, localities, `peer_by_name`, `ips_tagged`, `ips_in_domain`.

**Trace timeline** -- The events `SimulationLayer` captures: INFO and above, constant message, emitted inside a process or workload span. Each `TraceEvent` has `seq`, `time_ms`, `source`, `target`, `level`, `name` and typed fields.

**Watermark** -- For numeric assertions, the best value seen. For `assert_sometimes_greater_than!` the maximum, for `less_than` the minimum. An improvement on a sometimes watermark is a discovery.

**Worker** -- A forked process that runs exactly one exploration job and exits. Writes its discovery journal into a `MAP_SHARED` slot. Live processes are bounded by `1 + workers`; `workers: 0` runs in-process.

**Workload** -- The test driver. Survives process reboots, drives operations, keeps a reference model, checks results in `check()`. Registered with `workload_factory` or `workloads`. FoundationDB's `tester.actor.cpp`.
