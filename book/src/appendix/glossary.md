# Glossary

<!-- toc -->

Terms are listed alphabetically. Cross-references are shown in **bold**.

---

**Activation** -- The lifecycle event when an actor is first instantiated (or re-instantiated after **deactivation**). The `on_activate` hook runs before the actor handles its first message. Activation allocates resources and loads **persistent state**.

**Actor** -- A virtual actor: a single-threaded, identity-keyed message processor. Each actor has a unique **identity** string and handles one message at a time. Actors are created on demand and can be deactivated when idle. Inspired by Orleans.

**Adaptive forking** -- An exploration strategy where the number of **timelines** spawned at each **splitpoint** varies based on coverage yield. Productive **marks** that discover new coverage get more budget; **barren marks** return their energy to the **reallocation pool**. Configured via `AdaptiveConfig`.

**Always assertion** -- An assertion that must hold every time it is evaluated. Violations are recorded but do not panic, following the **Antithesis principle**. Checked by `validate_assertion_contracts()` after the simulation completes. See `assert_always!` and `assert_always_or_unreachable!`.

**Antithesis principle** -- The design philosophy that assertions should never crash the program. Violations are recorded and reported, allowing the simulation to continue and discover cascading failures. All 15 Moonpool assertion macros follow this principle.

**Attrition** -- Built-in chaos mechanism that randomly kills and restarts server **processes** during the chaos phase. Configured via the `Attrition` struct with probability weights for graceful, crash, and wipe reboots. Respects `max_dead` to limit simultaneous deaths.

**Barren mark** -- An assertion **mark** whose recent batch of **timelines** produced no new **coverage bitmap** bits. In **adaptive forking**, barren marks stop early and return their remaining **energy** to the **reallocation pool**.

**Buggify** -- Deterministic fault injection system inspired by FoundationDB's `BUGGIFY` macro. When enabled (50% activation rate, 25% firing rate per seed), buggified code paths randomly fire to test error handling. Decisions are deterministic given the **seed**, so bugs are reproducible.

**Chaos injection** -- The practice of deliberately introducing faults during simulation to test system resilience. Includes network partitions, connection failures, bit flips, clock drift, buggified delays, clogging, and process **attrition**. Configured via `ChaosConfiguration`.

**Coverage bitmap** -- A 1024-byte (8192-bit) bitfield that records which assertion paths a **timeline** touched. When an assertion fires, it sets a bit at position `hash(name) % 8192`. The **explored map** is the union of all coverage bitmaps across all timelines.

**Deactivation** -- The lifecycle event when an actor is removed from memory. The `on_deactivate` hook runs before the actor is dropped. Three hints control timing: `KeepAlive` (stays resident until shutdown), `DeactivateOnIdle` (deactivates after each message dispatch), and `DeactivateAfterIdle(Duration)` (deactivates after a period of inactivity).

**Determinism** -- The property that given the same **seed**, the simulation produces exactly the same execution. All randomness flows through the seeded RNG, and all I/O is simulated. This makes bugs reproducible: same seed, same bug, every time.

**Endpoint** -- A `(IpAddr, Token)` pair that uniquely identifies a connection endpoint in the simulated network. The IP address identifies the node; the **token** identifies the specific listener or connection on that node.

**Energy budget** -- A finite pool that limits how many **timelines** the **explorer** can spawn, preventing exponential blowup. In fixed-count mode, a single global counter. In **adaptive** mode, a 3-level system: global budget, per-**mark** budget, and **reallocation pool**.

**Explored map** -- The union (bitwise OR) of all **coverage bitmaps** across all **timelines**. Lives in `MAP_SHARED` memory so all forked processes can see it. Used to determine whether a new timeline discovered anything its siblings did not. Preserved across **seeds** in multi-seed exploration.

**Explorer** -- The multiverse exploration framework (`moonpool-explorer` crate). Uses `fork()` to create **timeline** branches at **splitpoints**, exploring alternate executions with different randomness. Has zero knowledge of Moonpool internals -- communicates only through RNG function pointers.

**Fork** -- An OS-level `fork()` call that creates a child process sharing the parent's memory via copy-on-write. Each child continues the simulation with a new **seed**, creating an alternate **timeline**. Forks are triggered at **splitpoints**.

**Frontier** -- For `assert_sometimes_all!`: the maximum number of named conditions that have been simultaneously true. When the frontier advances (more conditions true at once than ever before), a **splitpoint** is triggered. The frontier value is preserved across seeds in multi-seed exploration.

**Identity** -- The unique string key that identifies a virtual **actor**. Messages addressed to the same identity are routed to the same actor instance. Each identity has its own **mailbox** and lifecycle.

**Invariant** -- A property that must hold across the entire simulated system, checked after every simulation event. Invariants validate cross-**actor** or cross-**process** properties. Actors expose state via JSON through a `StateRegistry`; invariant functions read this state and panic on violation.

**Mark** -- An assertion site that can trigger **splitpoints** in the **explorer**. Each mark has a name, a shared-memory slot index, and (in **adaptive** mode) its own **energy** allowance. Marks are the unit of exploration budget management.

**Multiverse** -- The tree of all **timelines** explored from one root **seed**. Each **splitpoint** creates new children with different seeds. The multiverse is fully deterministic: given the same root seed and configuration, the same tree is produced.

**Placement** -- The mechanism that determines which node hosts a virtual **actor**. The default `Local` strategy activates actors on the node that first sends a message; `RoundRobin` distributes across cluster members. Custom `PlacementDirector` implementations can route actors based on **identity**, tags, or other criteria.

**Process** -- The system under test. A server node that can be killed and restarted (rebooted). Each process gets fresh in-memory state on every boot; persistence is only through storage. Created by a factory function registered via `SimulationBuilder::processes()`. Analogous to FoundationDB's `fdbd`.

**Provider** -- A trait abstraction over runtime services (time, tasks, network, random, storage). Real implementations (`TokioTimeProvider`, etc.) delegate to tokio; simulation implementations intercept calls for deterministic control. Code uses providers instead of calling tokio directly.

**Reachable** -- An assertion kind (`assert_reachable!`) that marks a code path as "should be reached at least once." On first reach, triggers a **fork**. A coverage violation is reported if the path is never reached after enough iterations.

**Reallocation pool** -- In **adaptive forking**, a shared energy reserve fed by **barren marks** that return their unused per-mark budget. Productive marks can draw from this pool when their own budget runs out, enabling automatic resource redistribution.

**Recipe** -- The sequence of **splitpoints** that leads to a specific **timeline**. Encoded as a list of `(rng_call_count, child_seed)` pairs. If a bug is found, the recipe enables exact replay via `SimulationBuilder::replay_recipe()`. Formatted as `"151@seed -> 80@seed"`.

**Seed** -- A `u64` value that completely determines a simulation's randomness. Same seed = same RNG sequence = same execution. Seeds can be set explicitly via `set_debug_seeds()` or generated automatically. The seed is the fundamental unit of reproducibility.

**Sometimes assertion** -- An assertion that should hold **at least once** across all iterations. Does not panic if false; instead, records statistics. On first success, triggers a **fork** in **exploration** mode. A coverage violation is reported if the condition is never true. See `assert_sometimes!`.

**Splitpoint** -- A moment where the **explorer** decides to branch the **multiverse**. Occurs when a **sometimes** assertion succeeds for the first time, a numeric **watermark** improves, or a **frontier** advances. The RNG call count at the splitpoint, combined with the **seed**, identifies the exact program state.

**Timeline** -- One complete simulation run. A **seed** plus a sequence of **splitpoints** uniquely identifies a timeline. The root timeline runs from the original seed; child timelines branch off at splitpoints with derived seeds.

**Token** -- A `u64` identifier for a specific listener or connection on a node. Combined with an IP address to form an **endpoint**. See also **well-known token**.

**Virgin map** -- An alternative name for the **explored map** (borrowed from AFL/fuzzing terminology). Records which coverage bits have been seen across all **timelines** and all **seeds**. "Virgin" bits are those never set by any timeline.

**Warm start** -- In multi-seed exploration, when a new **seed** begins with the **explored map** already containing coverage from previous seeds. Warm starts use a lower `warm_min_timelines` threshold for **barren mark** detection, since much coverage is already known. Enabled automatically by `prepare_next_seed()`.

**Watermark** -- For numeric **sometimes** assertions: the best value ever observed. For `gt`/`ge`, the watermark tracks the maximum; for `lt`/`le`, the minimum. When a new evaluation improves the watermark, a **splitpoint** is triggered to explore timelines that might push the metric further.

**Well-known token** -- A reserved **token** in the range `0..WELL_KNOWN_RESERVED_COUNT` used for framework services. Well-known tokens provide stable endpoints for services like RPC registries without requiring dynamic discovery.

**Wire format** -- The on-the-wire message encoding used by moonpool-transport. Each `WireMessage` includes a `WireHeader` with endpoint routing, a unique ID, message type, and payload size, followed by the serialized payload. CRC32C checksums protect against **bit flip** corruption.

**Workload** -- The test driver. A workload survives process **reboots** and drives requests against the system under test. It validates correctness by making assertions about observed behavior. Analogous to FoundationDB's `tester.actor.cpp`.
