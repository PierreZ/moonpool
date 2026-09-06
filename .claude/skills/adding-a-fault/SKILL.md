---
name: adding-a-fault
description: Add or change a simulator fault in moonpool - a NetworkFault variant and its NetworkFaultMask bit, a StorageConfiguration knob, an Attrition regime - with its default/random_for_seed/swarm_for_seed sampling, its Chaos::BuggifyKnobs perturbation, its SimFaultEvent kind, recovery-mode behaviour after chaos_duration, tests, and the book's fault reference. Use when adding a fault family or chaos surface, changing a fault's probability or cooldown, or when a fault must be masked per seed.
---

# Adding a fault

A fault in moonpool is configuration data sampled per seed, applied by an
engine, and recorded as an event. Adding one touches all four places, and the
swarm model only works if the new fault can be switched off independently of
the others.

## Where each kind lives

| Surface | Config type and sampling | Applied in | Masked by |
|---|---|---|---|
| network | `network/config.rs`: the chaos section of `NetworkConfiguration` with `default()`, `random_for_seed()`, `swarm_for_seed()` | `network/sim/engine.rs` (+ `delay.rs`, `stream.rs`) | `NetworkFault` enum + `NetworkFaultMask` (a `u16` of bits: `Clog`, `Partition`, `BitFlip`, `RandomClose`, `ConnectFailure`, `ClockDrift`, `BuggifiedDelay`, `PairLatency`, `BlackHole`), set with `SimulationBuilder::network_fault_mask` |
| storage | `storage/config.rs`: `StorageConfiguration` with `random_for_seed()`, `swarm_for_seed()` | `storage/sim/engine.rs` | the swarm subset drawn per seed |
| attrition | `runner/process.rs`: `Attrition` (eight fields) with `swarm_for_seed()`, `AttritionScope`, `AttritionVictims` | `runner/process_manager.rs` | `victims` scopes the draw and `max_dead` to a group or tag |
| knobs | `Chaos::BuggifyKnobs` perturbs enabled network and storage knobs to extremes | the config samplers | per-seed buggify activation |

`ChaosMode::Random` enables the whole surface and randomizes intensities;
`ChaosMode::Swarm` enables a random subset of the surface's families per
seed, the rest fully off. That is what lets one seed be "only partitions"
and the next "only bit flips".

## Steps

1. **Read the reference first.** FoundationDB's `sim2.actor.cpp` /
   `Net2.actor.cpp` for connection-level faults, `AsyncFileNonDurable` and
   `AsyncFileChaos.h` for storage, TigerBeetle's `packet_simulator.zig` for
   packet-level ideas (moonpool stays TCP-level: connection faults, not
   packets). Copy the probability and cooldown shape they use and cite it in
   the field's doc, as `bit_flip_probability` cites `BUGGIFY_WITH_PROB(0.0001)`.
2. **Add the knob** to the config struct with a documented default, then to
   `random_for_seed` (an intensity drawn from the stream) and `swarm_for_seed`
   (on or off, then intensity). Draw a fixed number of times.
3. **Add the mask bit** for a network fault (`NetworkFault` variant, `bit()`,
   the `with`/`without`/`contains` const fns already generalize) and make the
   engine consult the mask before applying it. A fault that cannot be masked
   off breaks every consumer that needs a clean network for one axis.
4. **Apply it in the engine** through scheduled events, never by mutating
   another component's state; return effects and a `WakeBatch`.
5. **Record it**: a `SimFaultEvent` variant with a `kind()` string in
   `chaos/fault_events.rs`, so it appears in the timeline as `sim_fault` and
   an invariant can correlate it.
6. **Recovery mode.** After `chaos_duration` the world stops injecting new
   faults and heals partitions in force, but keeps persistent damage (closed
   connections, degraded pair latency, clock skew, rotted records). Decide
   which side your fault is on and make the cutoff honour it
   (`tests/recovery_mode.rs`).
7. **Tests**: a directed test under `tests/network/` or `tests/storage/`
   that the fault fires and is observable, plus the canary. Then
   `cargo xtask sim run-all`.
8. **Book**: `appendix/04-fault-reference.md`, and `part3-building/10-network-faults.md`
   or `11-storage-faults.md` (`/update-the-book`).

A fault must never make a run unwinnable: a queue that can hold no traffic or
a partition that never heals is a defeat of eventual synchrony, not a fault.
