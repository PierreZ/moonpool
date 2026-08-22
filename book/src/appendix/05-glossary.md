# Glossary

<!-- toc -->

Terms are listed alphabetically. Cross-references are shown in **bold**.

---

**Always assertion** -- An assertion that must hold every time it is evaluated. Violations are recorded but do not panic, following the **Antithesis principle**. Checked by `validate_assertion_contracts()` after the simulation completes. See `assert_always!` and `assert_always_or_unreachable!`.

**Antithesis principle** -- The design philosophy that assertions should never crash the program. Violations are recorded and reported, allowing the simulation to continue and discover cascading failures. All 15 Moonpool assertion macros follow this principle.

**Attrition** -- Built-in chaos mechanism that randomly kills and restarts server **processes** during the chaos phase. Configured via the `Attrition` struct with probability weights for graceful, crash, and wipe reboots. Respects `max_dead` to limit simultaneous deaths.

**Buggify** -- Deterministic fault injection system inspired by FoundationDB's `BUGGIFY` macro. When enabled (50% activation rate, 25% firing rate per seed), buggified code paths randomly fire to test error handling. Decisions are deterministic given the **seed**, so bugs are reproducible.

**Chaos injection** -- The practice of deliberately introducing faults during simulation to test system resilience. Includes network partitions, connection failures, bit flips, clock drift, buggified delays, clogging, and process **attrition**. Configured via `ChaosConfiguration`.

**Determinism** -- The property that given the same **seed**, the simulation produces exactly the same execution. All randomness flows through the seeded RNG, and all I/O is simulated. This makes bugs reproducible: same seed, same bug, every time.

**Trace timeline** -- The append-only log of trace events captured by `SimulationLayer`. Producers emit plain `tracing::*!` events with a constant message (the event name) and structured fields; no markers or derives. The `source` is attributed from the process/workload span the orchestrator wraps around each actor task; sim time is stamped automatically. Invariants read via the `TraceQuery` trait: `q.since(name, &cursor)` for incremental scans, `q.snapshot(name)` for full re-scans, with per-field accessors (`e.u64("term")`, `e.str("leader")`). Each event carries `time_ms`, `source`, `target`, `level`, and a global monotonic `seq`. Distinct from **Timeline** (a simulation run in the explorer); see also **Fault timeline**.

**Endpoint** -- A `(IpAddr, Token)` pair that uniquely identifies a connection endpoint in the simulated network. The IP address identifies the node; the **token** identifies the specific listener or connection on that node.

**Explorer** -- The frontier-based exploration controller (`moonpool-explorer` crate). Owns the **frontier**, the per-state **exemplar** store, novelty bookkeeping, and a bounded pool of **worker** processes. Has zero knowledge of Moonpool internals -- communicates through the assertion accounting hooks and an RNG call-count function pointer.

**Fault timeline** -- The well-known event name `"sim_fault"` (`SIM_FAULT_EVENT_NAME`) in the trace timeline. The engine records `SimFaultEvent`s internally and the runner merges them in with `source = "sim"` and a `kind` field (e.g. `"partition_created"`, `"process_force_kill"`) covering network, storage, and process lifecycle faults. Invariants use it to correlate application behavior with infrastructure events. Engine-level tests can drain records directly via `SimWorld::take_faults()`.

**Fork** -- An OS-level `fork()` call used by the **explorer** to execute one exploration job cheaply via copy-on-write. Forks happen at a quiescent point between runs, never mid-simulation, and each **worker** exits after a single **timeline** -- the process tree is never the exploration tree.

**Discovery** -- A globally-new interesting state observed by the assertion accounting: the first pass of a **sometimes assertion**, a new `assert_sometimes_each!` bucket, or a **watermark**/**frontier (assertion)**/quality improvement. Each distinct discovery is latched by an atomic compare-and-swap in the shared assertion region, so it is journaled exactly once across all **timelines** and **seeds**.

**Exemplar** -- A retained **recipe** (plus the RNG call count of the discovery it anchors to) that reaches one semantic state. The **explorer** keeps a small bounded number per state, evicting oldest-first, because two executions can hit the same state with very different future potential.

**Frontier (assertion)** -- For `assert_sometimes_all!`: the maximum number of named conditions that have been simultaneously true. Advancing it is a **discovery**. Preserved across seeds.

**Frontier (exploration)** -- The **explorer**'s FIFO queue of jobs (recipes) waiting to be executed. Bounded by `max_frontier`.

**Invariant** -- A property that must hold across the entire simulated system, checked by the runner after every simulation step. Invariants read from the **trace timeline** via the `TraceQuery` trait and report violations via `assert_always!`. Registered on the builder with `.invariant(...)` or `.invariant_fn(...)`.

**Multiverse** -- The logical tree of all **timelines** explored from one root **seed**. Nodes are execution states, edges are reseed decisions. The multiverse is a data structure of recipes, not of live processes: it can hold thousands of timelines while at most `1 + workers` OS processes exist.

**Process** -- The system under test. A server node that can be killed and restarted (rebooted). Each process gets fresh in-memory state on every boot; persistence is only through storage. Created by a factory function registered via `SimulationBuilder::processes()`. Analogous to FoundationDB's `fdbd`.

**Provider** -- A trait abstraction over runtime services (time, tasks, network, random, storage). Real implementations (`TokioTimeProvider`, etc.) delegate to tokio; simulation implementations intercept calls for deterministic control. Code uses providers instead of calling tokio directly.

**Reachable** -- An assertion kind (`assert_reachable!`) that marks a code path as "should be reached at least once." First reach is a **discovery**. A coverage violation is reported if the path is never reached after enough iterations.

**Recipe** -- The replay path to a specific **timeline**: a list of `(rng_call_count, seed)` breakpoints applied from the root seed. If a bug is found, the recipe enables exact replay via `SimulationBuilder::replay_timeline()`. Formatted as `"151@seed -> 80@seed"`.

**Seed** -- A `u64` value that completely determines a simulation's randomness. Same seed = same RNG sequence = same execution. Seeds can be set explicitly via `set_debug_seeds()` or generated automatically. The seed is the fundamental unit of reproducibility.

**Sometimes assertion** -- An assertion that should hold **at least once** across all iterations. Does not panic if false; instead, records statistics. Its first success is a **discovery** that exploration can anchor continuations to. A coverage violation is reported if the condition is never true. See `assert_sometimes!`.

**SimStorageProvider** -- The simulation implementation of `StorageProvider`. Constructed with an IP address (`SimStorageProvider::new(sim, ip)`) so all file operations are tagged with the owning **process**. Fault injection uses the per-process `StorageConfiguration` resolved by `StorageState::config_for(ip)`.

**Timeline** -- One complete simulation run. A root **seed** plus a **recipe** uniquely identifies a timeline; the root timeline has an empty recipe.

**Token** -- A `u64` identifier for a specific listener or connection on a node. Combined with an IP address to form an **endpoint**. See also **well-known token**.

**Watermark** -- For numeric **sometimes** assertions: the best value ever observed. For `gt`/`ge`, the watermark tracks the maximum; for `lt`/`le`, the minimum. An improvement is a **discovery** and marks the owning state as monotonic progress, which continuation scheduling prefers. Preserved across seeds.

**Well-known token** -- A reserved **token** in the range `0..WELL_KNOWN_RESERVED_COUNT` used for framework services. Well-known tokens provide stable endpoints for services like RPC registries without requiring dynamic discovery.

**Worker** -- A forked process that executes exactly one exploration job (replay + continuation), writes its discovery journal into a `MAP_SHARED` result slot, and exits. Workers never fork and never make exploration decisions; the pool size (`workers`) bounds live processes. `workers: 0` runs jobs in-process, sequentially and fully deterministically.

**Wire format** -- The on-the-wire message encoding used by moonpool-transport. Each `WireMessage` includes a `WireHeader` with endpoint routing, a unique ID, message type, and payload size, followed by the serialized payload. CRC32C checksums protect against **bit flip** corruption.

**Workload** -- The test driver. A workload survives process **reboots** and drives requests against the system under test. It validates correctness by making assertions about observed behavior. Analogous to FoundationDB's `tester.actor.cpp`.
