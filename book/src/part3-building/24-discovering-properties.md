# Discovering Properties

<!-- toc -->

You know the fifteen macros. You know `buggify!` and `hint!`. The hard question with a fresh process and workload is not **how** to assert. It is **what** to assert, and **where** a fault would hurt most.

One read through the code finds the obvious properties. The ones that catch real bugs hide from a single viewpoint. The fix is to read the same code several times, each time through one lens.

## The Attention Focus Pattern

The idea comes from Antithesis's property-discovery work. Instead of one pass with a notebook, make eight passes. Each pass looks for one failure family and ignores the rest. Each pass is an **attention focus**.

Why does it work? Different bugs live in different mental models. A reader thinking about crash recovery does not notice a check-then-act race. A reader thinking about races does not notice an unsynced write. The discipline is the repetition: eight narrow readings beat one wide one.

## Eight Focuses

Each focus has a question, the moonpool tools that test it, and the assertion it usually produces.

**1. State integrity.** What must survive a reboot? Which numbers only go up? Which derived state can drift from its source? Tools: `assert_always!` on monotonic values, a `StateHandle` model published from the workload and compared in `check`, and an [invariant](./17-events-and-invariants.md) when the property spans nodes. The leader-election test is this focus: one leader per term.

**2. Concurrency.** Moonpool runs one OS thread, but every `.await` is an interleaving point. Two workloads hit the same key. A task reads an `Arc<RwLock<..>>`, yields, then writes. Tools: `assert_always!` on read-after-write from each workload's view, and `swarm_operations()` so the operation mix differs per seed.

**3. Crash recovery.** A process factory returns a blank instance. In-memory state is gone, and an unsynced write may be torn. Tools: `Chaos::Attrition` for reboots, `SimContext::crash_self` for a crash the code chooses, and `hint!("label").await` in shipped code at the moment that matters. moonpool-journal names three such moments inside one commit: `entries written, not synced`, `records written, not synced`, `batch synced, metainfo stale`. The simulator decides whether to strike each one. Assert with `assert_sometimes!` that recovery ran, and `assert_always!` that it rebuilt the right state.

**4. Network faults.** A call without a timeout. A retry that is not idempotent. A cached leader address after a partition heals. Tools: `Chaos::Network(ChaosMode::Swarm)` with a `network_fault_mask`, and the `sim_fault` timeline to correlate. Assert with `assert_always!` that the applied count equals the acknowledged count, and `assert_reachable!` on the retry branch.

**5. Timing and scheduling.** An election timeout that overlaps a heartbeat timeout. Logic that assumes a timer fires before a message arrives. Tools: `buggify!` to stretch a delay, `moonpool_sim::select!` so branch order is seeded, `Chaos::GrayFailure` and `SimContext::slowness` for a slow machine. Assert with `assert_sometimes!` that the late branch won at least once.

**6. Resource boundaries.** A queue that grows forever under load. No backpressure. Tools: the numeric macros. `assert_always_less_than!` guards the bound. Its `assert_sometimes_greater_than!` twin makes the explorer push toward it. `SystemProvider` counters and [application metrics](./25-metrics.md) show the shape of the pressure.

**7. Protocol contracts.** A doc comment says "returns an error if missing" and the code returns a default. An ordering rule between two RPCs that nothing checks. Tools: `assert_always!` at the boundary, and `assert_unreachable!` on the branch the contract says cannot happen.

**8. Lifecycle transitions.** A request arrives before setup finished. Graceful shutdown drops in-flight work. State is published before it is valid. Tools: `RebootKind::Graceful` with a grace period, `Chaos::Outage` for a correlated group outage, `assert_sometimes_each!("phase", [("phase", phase as i64)])` to prove every phase was entered.

## Running the Passes

Two ways work.

**Sequential.** Walk the focuses as a checklist. Read the whole process once per focus. Do not skip a focus because an earlier pass "already covered" that function. The same line looks different under crash recovery and under concurrency, and that difference is the point.

**Ensemble.** Give each focus to a separate reviewer, human or AI agent, with only that focus. Merge the results. A property found by three reviewers is high confidence. A property found by one is the kind a single pass would have missed, and often the best catch of the exercise.

Either way, write each finding down the same way:

- **Location**: file and line range.
- **Macro**: `assert_always!`, `assert_sometimes!`, `hint!`, `buggify!`, and so on.
- **Message**: unique, constant, under 64 bytes.
- **Rationale**: the bug this catches.
- **Provenance**: which focus found it.

Then implement, run with the default `UntilCoverageStable`, and read the report. A `sometimes` that stays at 0% is a finding too: chaos never reached that branch, so either the fault mix is wrong or the branch is dead.

## A Short Example

Take a key/value process that accepts writes over RPC and persists them. One pass adds `assert_always!(stored == written, "read-after-write")` and stops.

The focuses add four more:

- **State integrity**: the process keeps a write counter. `assert_always!(new > old, "write counter never regresses")`.
- **Crash recovery**: writes land in a map and flush to disk on a timer. Put `hint!("kv: written, not flushed").await` between the two. Add `assert_reachable!("kv: recovery replayed the flush log")`.
- **Concurrency**: two workloads write the same key. `assert_always!` on last-write-wins from each workload's view.
- **Network faults**: the client retries on timeout and the write is not idempotent. `assert_always_less_than_or_equal_to!(applied, acknowledged, "a retry never double-applies")`.

The last one is the production outage. A single pass would have found the first.

Properties are not static. Each time a seed fails, ask which focus would have predicted it, and add that focus's missing assertion before you fix the bug. The assertion stays. The bug does not come back.
