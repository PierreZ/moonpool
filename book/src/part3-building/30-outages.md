# Correlated Outages and Self-Crashes

<!-- toc -->

## The Whole Rack Goes Dark

Attrition never takes more than `max_dead` processes at once, and that is the point: it models the steady drizzle of failures a healthy cluster rides through. A rack-wide power loss is a different fault. Every process of a group loses power **at the same instant**. Every memory is gone. What survives is exactly what the disks hold, and no peer can repair a copy from memory, because no peer is awake. Then the machines come back one by one, each on its own schedule, and one straggler takes much longer than the rest.

FoundationDB's simulator models this as `killDataCenter` in `sim2.actor.cpp`: it kills every machine of a datacenter in one loop and lets each machine's own reboot delay bring it back. A quorum protocol that never faces it has never had its recovery path tested from cold disks alone.

## Chaos::Outage

```rust
use std::time::Duration;
use moonpool_sim::{Chaos, ChaosMode, Outage, SimulationBuilder};

SimulationBuilder::new()
    .processes(3, || Box::new(Acceptor::new()))
    .processes(2, || Box::new(Matchmaker::new()))
    .enable_chaos([Chaos::Outage {
        config: Outage {
            start: Duration::from_secs(10)..Duration::from_secs(25),
            down: Duration::from_secs(1)..Duration::from_secs(5),
            straggler: Some(Duration::from_secs(20)..Duration::from_secs(40)),
            ..Outage::groups(["acceptor"])
        },
        mode: ChaosMode::Swarm,
    }])
    .chaos_duration(Duration::from_secs(30))
```

`Outage::groups` names the process groups that lose power together (their `Process::name`). It sets `probability` to 1.0, `start` to the first ten seconds of the window, `down` to one to ten seconds, and no straggler. Override the fields you care about.

| Field | Meaning |
|-------|---------|
| `groups` | The groups whose live processes crash together. Must name registered groups. |
| `probability` | Whether a seed has an outage at all. In `0..=1`. |
| `start` | When the outage strikes, measured from the opening of the chaos window. A draw past the window's end means no outage. |
| `down` | How long each victim stays down, drawn per victim. |
| `straggler` | How long the one straggler stays down. Must start at or after the end of `down`, so the straggler is always the last one back. |

The builder validates the regime before the first seed. An unregistered group, a probability outside `0..=1`, a range that ends before it starts, or a straggler range inside `down` returns `SimulationError::InvalidConfiguration` from `run()`. So does an outage registered without `chaos_duration`.

At most once per run, with `probability`, at a time drawn from `start`, every live process of the named groups crashes in the same tick. Each one restarts after its own draw from `down`. With a `straggler` range, one victim, picked at random, is held down for a draw from that range instead. A process already dead at the outage instant is not a victim: it comes back on its own schedule. A `.cluster()` group whose per-seed count drew zero has no live victim, so the outage lands on no one and the run goes on.

`ChaosMode::Random` keeps the outage on every seed, still subject to `probability`. `ChaosMode::Swarm` drops the outage on half the seeds and the straggler on half the rest, in two draws made whether or not the outage survives.

## Why the Fault Is Trustworthy

Two properties make the outage safe to build on.

Every draw comes from the simulation stream. The probability and start are drawn when the window opens. The delays are drawn at the outage instant, in ascending IP order. A seed or an exploration recipe replays **the same kill and restart schedule**.

Each restart is a scheduled `ProcessRestart` event, not a step of the injector. A restart lands even after the chaos window closes and the injector is gone, so a victim is never stranded down into the recovery tail. Victims count toward `dead_count`, so attrition holds off while they are down. Give the workload enough time after the window for the last straggler to come back.

## OutageLanded: Acting at the Instant No Copy Lives

The injector tells you when it has landed. Once every victim's kill has run, it publishes an `OutageLanded` under `OUTAGE_STATE_KEY` in the run's `StateHandle`:

```rust
use moonpool_sim::{OUTAGE_STATE_KEY, OutageLanded};

if let Some(landed) = ctx.state().get::<OutageLanded>(OUTAGE_STATE_KEY) {
    // landed.at: the instant the kills were decided.
    // landed.victims: every victim, ascending IP order.
    // landed.down: each victim's time down, parallel to victims.
    // landed.straggler: the one held longest, if any.
}
```

A custom injector that waits for that key can plan latent disk damage at the one moment no copy lives in memory anywhere. A process can read it after its own reboot to learn it came back from a group-wide cut.

## OutageVeto: A Budget the Simulator Cannot See

The harness can hold an outage back. A replicated store with a commit in flight would be left ambiguous by a cut, and only the harness knows a commit is in flight. Publish an `OutageVeto` under `OUTAGE_VETO_KEY`:

```rust
use moonpool_sim::OutageVeto;

// From a fault injector at the start of the window.
let ledger = ledger.clone();
OutageVeto::new(move |victims| ledger.safe_to_cut(victims)).publish(ctx.state());
```

At the outage instant, before it draws anything, the injector asks the veto with every victim's IP in ascending order. A `true` answer is an outage: every victim crashes at once, so the harness may record it as done. A `false` answer leaves every victim alive, and the injector asks again `VETO_RETRY` (1 ms) later, with the victims live at that moment, until the veto permits the outage or the chaos window closes. The veto must not draw randomness, because its answer must not move the draw schedule.

It is the outage's form of `HintVeto`: one answer for the whole victim set, so a budget that counts victims together is judged once. `crates/moonpool-sim/tests/outage.rs` shows a veto that holds the outage until a chosen instant and records every set it was asked about.

## Self-Crashes: A Process Chooses the Instant

An outage and attrition are the environment's decisions. A `hint!` is a request the simulator may refuse. Sometimes a test needs a crash at an exact point of a process's own protocol, with no regime in the way: mid-append, between two syncs, right after a lease is written. `SimContext::crash_self` is that command.

```rust
use std::time::Duration;
use moonpool_sim::RebootKind;

file.write_at(0, &new_sectors).await?;
// The kill lands one tick from now, while the sync is in flight.
ctx.crash_self(RebootKind::Crash, Some(Duration::from_millis(50)))?;
file.sync_all().await?; // never completes
```

The call schedules the same force kill a fault injector's `reboot` or `crash` does, one scheduler tick away. The process's tasks abort, its connections reset, and its storage goes through crash resolution, so every write not yet synced, including the one whose sync is in flight, resolves by crash physics. `RebootKind::CrashAndWipe` also wipes the disk. Call it, then keep awaiting the work it interrupts: the kill lands before that work's next storage completion.

With `Some(delay)` the process restarts that long after the kill. With `None` it stays down until a fault injector restarts it. No randomness is drawn, and the process counts as dead until it restarts. Two calls are errors: `RebootKind::Graceful` (a process shuts down gracefully by returning) and a call from a workload (only a process can crash itself).

Code that holds no `SimContext`, such as a driver's hooks generic over `Providers`, takes a handle at boot:

```rust
// A SelfCrash is Clone + Send + Sync + 'static, so it rides into a hook.
let handle = ctx.self_crash()?;
handle.crash(RebootKind::Crash, Some(Duration::from_millis(10)))?;
```

`crates/moonpool-sim/tests/crash_self.rs` crashes a process between a write and its sync and checks, on the next boot, that every sector resolved to one of the two values the barrier-bounded crash model allows. Use a self-crash when the test is the point. Use a `hint!` when the shipped code should name the moment and the seed should decide.
