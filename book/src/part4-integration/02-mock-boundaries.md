# Where to Draw the Line
<!-- toc -->

When someone says "fake your dependencies", it sounds like cutting corners.
Real databases catch real bugs. Test containers give you the real thing. A
`BTreeMap` that pretends to be Postgres looks like a compromise.

That instinct is wrong. **Fakes are not a compromise. They are more powerful
than real dependencies**, because you control their failures.

## The Partial Failure Problem

A test container gives you binary failure. The whole service is up, or the
whole service is down. Docker kills the container, your test sees "connection
refused", and you verify that your retry logic works.

Production failures are never binary. Kafka loses partition 3 while partitions
1, 2 and 4 stay healthy. A Postgres replica lags 800 ms behind the primary
while the primary is fine. One Redis shard runs out of memory while five others
serve traffic. Your S3 bucket returns 503 on 2% of PUTs while GETs succeed at
full speed.

These partial failures are where bugs hide. The request that reads from the
lagging replica and writes to the healthy primary. The consumer that
rebalances and loses its offset for exactly one topic. The cache lookup that
fails for one key prefix while the rest of the keyspace works.

A container **cannot produce these failures**. It is a black box. You can start
it or stop it. You cannot reach inside and make partition 3 fail while
partition 4 succeeds.

## Fakes Give You Control

A trait-based fake controls the failure surface at any granularity. Your
`MessageBroker` fake can return `Ok` for partition 1 and `Err` for partition 3
in the same call. Your `Database` fake can add 200 ms of latency on reads from
replica 2 while replica 1 answers at once.

Combined with moonpool's buggify macros, a fake becomes a **probabilistic
fault injector**. Every operation has a chance to fail, and the chance is
drawn from the seeded RNG. When a seed fails, you replay it and get the same
sequence of partial failures.

```rust
impl Store for InMemoryStore {
    fn create(&self, name: &str) -> Result<Item, StoreError> {
        // Active at this site in about half the seeds, then fires at 25%
        // per call. A real Postgres is up or down; this fake can fail
        // one write and let the next read succeed.
        if moonpool_sim::buggify!() {
            return Err(StoreError::WriteFailed("buggified".into()));
        }
        // normal path
    }
}
```

Three macros cover most fakes. `buggify!()` fires at 25% once the site is
active. `buggify_with_prob!(0.05)` sets the per-call rate. `buggify_named!("replica-lag", 1.0)`
keys the site by a label instead of its `file:line`, so a harness can switch
that failure family on or off per seed with `set_activation`. That is how a
single fake joins [swarm testing](../part3-building/28-swarm-testing.md): each
seed enables a random subset of failure families, and the seeds that fail tell
you which combination matters.

The macros come from `moonpool-buggify`, a crate with zero dependencies. Outside
a simulation every site evaluates to `false` with no side effects. A fake that
ships in a production binary (a local development mode, for example) costs
nothing. [Buggify](../part3-building/08-buggify.md) covers the activation
model in detail.

## The Fidelity Spectrum

Not every dependency needs the same fidelity. Think of fakes on a spectrum:

| Level | What it does | Use it for |
|-------|--------------|------------|
| **No-op** | Returns `Ok(())` for everything | Logging, metrics, tracing facades |
| **In-memory** | `BTreeMap` storage, `VecDeque` queues, correct behavior, no persistence | Most unit and integration tests |
| **Fault-injectable** | In-memory plus buggify sites on each operation | Most simulation fakes |
| **Full simulation** | The dependency runs as a `Process` on the simulated network | Your own services, consensus protocols |

Most fakes live on the third row. The fourth row is for components where the
network interaction **is** the behavior under test.

## Per-Dependency Guidance

**Network (HTTP, gRPC, TCP)**: simulate with moonpool. Real HTTP parsing, real
serialization, simulated transport. Your handlers run unchanged.

**Database**: a trait with a `BTreeMap` behind it. Model the operations your
code uses (CRUD, transactions, queries by index). Inject failures per
operation. Do not simulate SQL parsing.

**Message brokers**: a trait with per-partition control. One `VecDeque<Message>`
per partition and a buggify site per partition. Model consumer group
rebalancing only if your code depends on it.

**External HTTP APIs**: canned responses with injectable failures. Your Stripe
client fake returns a known charge object, and `buggify_with_prob!(0.1)` turns
it into a 429 or a timeout.

**Cache**: a trait with a `BTreeMap`. Inject evictions and connection failures.

**Local disk**: not a fake. Write against `StorageProvider` and let the
simulator supply the disk, crashes included. See
[Storage Faults](../part3-building/11-storage-faults.md).

## Rules of Engagement

**`BTreeMap`, not `HashMap`.** Iteration order must be the same on every run.
A `HashMap` iterates in a different order per process, and that makes the
simulation non-deterministic.

**`Send + Sync` state.** axum requires `State` to be `Send + Sync`, and every
moonpool trait is Send-bounded. Use `Arc<RwLock<BTreeMap<…>>>`, not
`Rc<RefCell<…>>`. The executor runs on one OS thread, so the lock is never
contended. The bounds make the type fit, not the thread count.

**No wall clock, no OS entropy inside the fake.** A fake that calls
`Instant::now()` or `rand::random()` breaks replay. Take time and randomness
from the providers, or from the buggify macros.

**The Oxide convention.** Oxide's [omicron](https://github.com/oxidecomputer/omicron)
and [crucible](https://github.com/oxidecomputer/crucible) keep fakes in a
`fakes/` module next to the real implementation. The trait and the fake ship
together. When the trait changes, the fake changes in the same PR.

## The 80% Argument

A fake that covers 80% of a dependency's behavior, with determinism and fault
injection, is **strictly better** than a container that covers 100% with no
control over failures.

Containers are non-deterministic. They take seconds to start. They need Docker.
They break across versions. They cannot produce partial failures. A failing
test gives you a log and a guess.

A fake compiles with your project. It runs in microseconds. It reproduces from
a seed. It produces failures that containers cannot. The 80% you model covers
the behavior your code depends on.

The other 20% belongs in separate integration tests against real
infrastructure, run less often. Those tests verify that the fakes are faithful.
The simulation tests, run on every commit, verify that your code handles
failure. When an integration test reveals a failure mode the fake does not
model, add it to the fake. When a simulation finds a bug under partial failure,
confirm the fix against the real thing. The two kinds of tests improve each
other.
