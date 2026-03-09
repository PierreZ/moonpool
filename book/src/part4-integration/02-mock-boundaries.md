# Where to Draw the Line
<!-- toc -->

The instinct when someone says "fake your dependencies" is to feel like you're cutting corners. Real databases catch real bugs. Test containers give you the real thing. A BTreeMap pretending to be Postgres is a compromise.

That instinct is wrong. **Fakes aren't a compromise. They're more powerful than real dependencies.**

## The Partial Failure Problem

Test containers give you binary failure: the whole service is up, or the whole service is down. Docker kills the container, your test sees a connection refused, you verify your retry logic works.

Production failures are never binary. Kafka loses partition 3 while partitions 1, 2, and 4 stay healthy. A Postgres replica lags 800ms behind the primary while the primary is fine. One Redis shard OOMs while five others serve traffic normally. Your S3 bucket returns 503 on 2% of PUTs while GETs succeed at full speed.

These partial failures are where bugs hide. The request that reads from the lagging replica and writes to the healthy primary. The consumer that rebalances partitions and loses its offset for exactly one topic. The cache lookup that fails for one key prefix while the rest of the keyspace works.

Test containers **cannot produce these failures**. A container is a black box. You can start it or stop it. You cannot reach inside and make partition 3 return errors while partition 4 succeeds.

## Fakes Give You Control

A trait-based fake controls the failure surface at arbitrary granularity. Your `MessageBroker` trait fake can return `Ok` for partition 1 and `Err` for partition 3 in the same call. Your `Database` fake can inject 200ms latency on reads from replica 2 while replica 1 responds instantly. Your `Cache` fake can evict entries for keys matching a pattern while retaining everything else.

Combined with moonpool's `buggify!()` macro, these fakes become **probabilistic fault injectors**. Every operation has a chance of failure, controlled by a deterministic seed. When a test fails, you replay the exact same seed and get the exact same sequence of partial failures.

```rust
impl Store for InMemoryStore {
    fn create(&self, name: &str) -> Result<Item, StoreError> {
        // 25% chance of write failure when buggify is enabled.
        // A real Postgres can only be fully up or fully down.
        // This fake can fail individual writes.
        if buggify!() {
            return Err(StoreError::WriteFailed("buggified".into()));
        }
        // ... normal implementation
    }
}
```

## The Fidelity Spectrum

Not every dependency needs full simulation. Think of fakes on a spectrum:

**No-op**: Returns `Ok(())` for everything. Useful when you don't care about a dependency's behavior, just that calls don't crash. Logging, metrics, tracing facades.

**In-memory**: BTreeMap-based storage, VecDeque-based queues. Correct behavior without persistence. Good for most unit and integration tests.

**Fault-injectable**: In-memory with `buggify!()` on operations. Correct behavior most of the time, controllable failures when chaos is enabled. This is where most simulation fakes live.

**Full simulation via moonpool**: The dependency runs as a Process in the simulation. Network traffic is simulated with latency, drops, and corruption. Reserved for components where the network interaction IS the interesting behavior (your own services, consensus protocols).

## Per-Dependency Guidance

**Network (HTTP, gRPC, TCP)**: Simulate via moonpool. This is the sweet spot. Real HTTP parsing, real serialization, simulated transport. Your handlers run unchanged.

**Database**: Trait fake with BTreeMap. Model the operations your code actually uses (CRUD, transactions, queries by index). Inject failures per-operation. Don't try to simulate SQL parsing.

**Message brokers**: Trait fake with per-partition control. A `VecDeque<Message>` per partition, with injectable failures per-partition. Model consumer group rebalancing if your code depends on it.

**External HTTP APIs**: Canned responses with injectable failures. Your Stripe client fake returns a known charge object, but `buggify!()` returns a 429 or network timeout 10% of the time.

**Cache**: Trait fake with partial cluster modeling if you need it, simple HashMap if you don't. Inject evictions and connection failures.

## Rules of Engagement

**BTreeMap, not HashMap**. Deterministic iteration order matters for reproducibility. A HashMap iterating in different order across runs makes your simulation non-deterministic.

**Send + Sync for axum State**. Axum requires `State` to be `Send + Sync`. Your fakes need `Arc<RwLock<BTreeMap<...>>>`, not `Rc<RefCell<...>>`. This is actually fine because the simulation runs single-threaded anyway. The lock is never contended.

**The Oxide convention**. Oxide's [omicron](https://github.com/oxidecomputer/omicron) and [crucible](https://github.com/oxidecomputer/crucible) projects keep fakes in a `fakes/` module alongside the real implementation. The trait and the fake ship together. When you change the trait, you update the fake in the same PR. This keeps fakes from drifting.

## The 80% Argument

A fake covering 80% of a dependency's behavior with determinism and fault injection is **strictly better** than a test container covering 100% with no control over failures.

Test containers are non-deterministic. They're slow (seconds to start). They require Docker (not available in all CI environments). They break across versions (Postgres 15 vs 16 container images). They can't produce partial failures. A failing test gives you a log and a prayer.

A fake compiles with your project. It runs in microseconds. It reproduces from a seed. It produces failures that containers physically cannot. The 80% you model covers the behavior your code actually depends on.

The remaining 20% you don't model? That belongs in separate integration tests, run less often, against real infrastructure. Nightly CI with actual Postgres and Kafka containers. Those tests verify your fakes are faithful. The simulation tests, run on every commit, verify your code handles failure correctly.

Both kinds of tests improve each other. When an integration test reveals a failure mode your fake doesn't model, you add it to the fake. When a simulation test finds a bug under partial failure, you verify the fix against real infrastructure. The two approaches compound.
