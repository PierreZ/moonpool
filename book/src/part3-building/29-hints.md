# hint!: Naming Fault Sites

<!-- toc -->

## The Code Names a Moment, the Simulator Decides

Attrition reboots a process wherever a task happens to yield. Volume alone rarely lands a reboot on the one await that matters: the batch is synced, the acknowledgements are not sent yet. That edge is one await wide. The code knows where it is. The simulator does not.

FoundationDB's server code solves this with `if (buggify()) throw please_reboot()`, and its simulated worker turns the throw into a reboot. The site is precise, and the site chooses the fault. Moonpool keeps the precision and moves the choice: **the code names a moment. It never names a fault.**

```rust
use moonpool_buggify::hint;

storage.sync().await?;
hint!("batch durable, not sent").await;
send(messages).await;
```

The label is a human string and a coverage bucket. It carries no semantics the simulator acts on. The simulator strikes the **calling process** if the seed's chaos allows it. Today every strike is a reboot, the one fault that tests a storage moment and a network moment alike: it loses the unsynced bytes and the unsent messages at once.

## A No-Op in Production

`hint!` expands to a disruptive buggify site (`buggify_fault_internal`) with a label. Outside a simulation, buggify is disabled, so the site draws nothing, asks no one, and the returned future is ready at once. The cost is one thread-local read. `moonpool-buggify` has zero dependencies, so the same code ships to production and to `wasm32`. This is the rule from [Same Code in Production and Simulation](../part2-foundations/12-same-code.md): no `SimMachine`, no test-only hook, one binary.

## What a Strike Looks Like

`hint!` returns a `Hinted` future. The decision is taken when `hint!` is called, not when the future is polled. `Hinted::strike()` reports it as a `Strike`:

| `Strike` | Meaning | The future |
|----------|---------|------------|
| `None` | No fault fits, the seed's chaos forbids one, or the caller is no process. | Ready at once. |
| `Struck` | A fault the caller survives. Reserved for storage and network strikes. | Ready at once. |
| `Killed` | The calling process is killed. | Pending forever. |

On `Killed` the kill is scheduled one scheduler tick away through the same `ProcessForceKill` event a fault injector uses. The process's tasks abort, its connections reset, and every write not yet synced resolves by the disk's crash physics. Nothing after the `.await` runs. That is why the macro is `#[must_use]`: a hint you do not await lets the code after the moment run on a process that is already dead.

The simulator finds the process through the task being polled: every task carries the process that spawned it, across `spawn_task`. A hint from a workload, an injector or the driver has no owner, so the sink answers `None` and the hint resolves.

## Three Vetoes and a Fourth

Three vetoes stand between a hint and a reboot. None of them commands.

**1. The site.** A hint is a buggify location. It is activated once per run at the runner's 50%, and it fires per call at a low rate: `hint::POINT_PROB` (5%) for `hint!("label")`, or a literal at the site for `hint!("label", 0.2)`. A site at rate `1.0` is a per-seed scenario: activation is the draw. Once the chaos window closes the site is silent, like every `buggify_fault_with_prob!` point, and the runner removes the sink. The rate is a literal next to the moment it describes, never a knob.

**2. The seed's chaos.** The `HintSink` hands the decision to the first `Chaos::Attrition` regime whose `victims` filter admits the process. No regime covers the process? No strike. The regime's reboot-kind weights pick graceful, crash or crash-and-wipe. Its recovery delay applies. A seed whose swarm drew the never-reboot regime (`max_dead = 0`) never reboots on a hint either. Register attrition with `ChaosMode::Swarm` and the hints follow the same per-seed subset as the timed reboots.

**3. The budget.** The regime's `max_dead` counts dead processes among its victim pool. A hint that would exceed it is refused, exactly as a timed reboot would be. The hinting process alone is rebooted, whatever the regime's failure scope.

**4. The harness's budget.** Some budgets the simulator cannot see. A replicated store can lose copies at only so many replicas before a crash stops being survivable, and only the harness keeps that count. It publishes a `HintVeto` in the run's state. The sink asks it last, once a reboot fits the regime, so a `true` answer is a reboot and the harness can count it as spent:

```rust
use moonpool_sim::HintVeto;

// From a process at boot, or from a fault injector at the window's start.
let ledger = ledger.clone();
HintVeto::new(move |ip, _label| ledger.permit_cut(ip)).publish(ctx.state());
```

The veto runs inside the hint's call, so it must not draw randomness. `HINT_VETO_KEY` is the state key, and a later `publish` replaces an earlier veto.

The same veto guards the regime's own timed reboots of one process. A timed reboot lands at any instant, inside a commit too, so a budget that only hints asked would leak. Before each one, a per-process regime asks the veto with the label `TIMED_REBOOT_LABEL`. A `false` answer skips that round. A per-domain regime's group reboot does not ask it.

The chain costs nothing until a site fires. Every draw (activation, firing, kind, delay) goes through the one simulation stream from inside the call, so a hinted reboot replays exactly. `crates/moonpool-sim/tests/hint.rs` runs the chain under `check_determinism` and shows a refused veto leaving the process alive.

## Where to Put a Hint

A hint is worth placing where an environmental fault changes what the next boot finds. Two shapes cover most of them:

- **After a sync, before a send.** The data is durable, and no peer knows. A reboot here leaves the peers unaware of what this process holds.
- **After a write, before its sync.** The data is staged, not durable. A reboot here tears the write, and recovery must cope with a partial record.

Other good moments: a promise committed before its acknowledgement leaves, a prepare sent before its reply is handled, a metainfo update staged behind a synced batch. A bad moment is a hot path with no state at stake: the site fires often and finds nothing.

A hint is **not** for a decision of the code. Skipping a resend, shrinking a batch, taking a rare branch: those need no environment, so they are plain `buggify!` sites. Nothing is both.

## A Worked Example: moonpool-journal

`moonpool-journal` ships three hints inside its commit protocol (`crates/moonpool-journal/src/journal.rs`). The journal depends on `moonpool-core` and `moonpool-buggify` only, so every user of the journal gets these power cuts with no code of its own.

```rust
segment.write(batch.entry_at, &batch.entries).await?;
// A power loss here tears entries no record points at yet.
moonpool_buggify::hint!("journal commit: entries written, not synced", COMMIT_HINT_PROB).await;
if ordered {
    segment.sync().await?;
}
segment.write(batch.persist_at, &batch.records).await?;
// A power loss here may tear the records, rebuild them from their
// entries, or leave the last batch ambiguous.
moonpool_buggify::hint!("journal commit: records written, not synced", COMMIT_HINT_PROB).await;
segment.sync().await?;
if let Some(meta) = meta {
    // The batch is durable and the metainfo still names the old one.
    moonpool_buggify::hint!("journal commit: batch synced, metainfo stale", COMMIT_HINT_PROB).await;
    meta_file.update(meta).await?;
}
```

`COMMIT_HINT_PROB` is 0.25, the buggify default, not the 5% of a hot-path point: a commit is short, and a cut inside it is the case its recovery exists for. Each of the three moments maps to a recovery path the journal must take: a torn record, a record rebuilt from its entry, an ambiguous last batch. A harness that counts the replicas it can afford to lose publishes a `HintVeto`, and the journal never knows. [A Crash-Aware Journal](../part5-building-on-top/08-journal.md) follows those recoveries.

## What a Hint Is Not

A hint cannot aim at another process. The fleet killer, the rack outage and the scripted crash stay on the harness side, as [outages](30-outages.md) and [custom injectors](31-custom-faults.md). A process that must crash at an exact point of its own protocol, with no chaos regime in the way, uses `SimContext::crash_self` instead. That is a command, not a hint, and it is covered with the outages.

The design note behind this API weighed a named fault ("reboot me"), a weakness taxonomy and a resource-scoped statement, and rejected all three: each tells the simulator something it already owns, or lets production code choose a fault. A labelled moment is the smallest thing the code must say.
