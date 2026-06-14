# Network Faults

<!-- toc -->

## TCP, Not Packets

Moonpool simulates network faults at the **TCP connection level**, not the individual packet level. This is a deliberate design choice, inherited from FoundationDB. In practice, distributed systems rarely deal with individual packets. They deal with connections: connections that drop, connections that stall, connections that report success on one side and failure on the other. These are the faults that matter for application correctness.

Packet-level simulation (what TigerBeetle does) is useful for testing network stacks themselves. But for application-level distributed systems, connection-level faults exercise the code paths that actually fail in production: reconnection logic, request retries, leader election on disconnect, and state reconciliation after a partition.

## The Fault Catalog

Moonpool's `ChaosConfiguration` controls a wide range of network faults. Each fault is independently configurable and randomized per seed when using `NetworkConfiguration::random_for_seed()`.

### Latency Injection

Every network operation (bind, accept, connect, read, write) has a configurable latency range. The simulator picks a random duration from the range for each operation. This models the basic reality that network operations take time, and that time varies.

For tail latency testing, moonpool supports **bimodal latency distribution**, following FoundationDB's `halfLatency()` pattern. In bimodal mode, 99.9% of operations use normal latency, but 0.1% experience latencies multiplied by 5x to 20x. This is how real networks behave: most requests are fast, but a small fraction hit GC pauses, cross-datacenter hops, or congestion.

Re-sampling latency per operation has a blind spot: no single connection ever stays slow. Real clusters break differently. One machine sits behind a degraded switch port and every message to it lags, run after run, while the rest of the fleet is healthy. That **stably-slow peer** is exactly what stalls a quorum or reorders a consensus round, and uniform per-operation jitter never produces it. So moonpool borrows FoundationDB's `SimClogging` trick: set `max_pair_latency` to a range and each ordered IP pair draws **one** fixed latency at first contact, then carries it for the whole run. Off by default (`ZERO..ZERO`), it adds nothing. Turn it on and some pairs are permanently sluggish while others are quick, which is how you find the bug where the slow replica is the one holding the lease.

### Connection Drops

Random close injects spontaneous connection failures during I/O operations, at a configurable probability (default 0.001%). When triggered, 30% of closes are **explicit** (the caller gets an error) and 70% are **silent** (the connection just stops working). This ratio, taken from FoundationDB, tests both error-handling paths and timeout-based failure detection.

A cooldown period prevents cascading closes from overwhelming the system. The goal is to test recovery, not to make the system completely inoperable.

### Clogging

Write clogging stalls data delivery on a connection for a random duration (100-300ms by default). This simulates network congestion, TCP backpressure, and flow control contention. Code that assumes writes complete promptly will fail under clogging.

### Partial Writes

Writes are truncated to a random length (0 to 1000 bytes by default), following FoundationDB's approach. This tests TCP fragmentation handling and message framing logic. If your wire protocol assumes that a single write delivers a complete message, partial writes will break that assumption immediately.

### Bit Flips

Packet data is corrupted with random bit flips at low probability (0.01% by default). The number of flipped bits follows a power-law distribution between 1 and 32. This tests checksum validation and corruption detection. Without bit-flip injection, corruption bugs only surface in production when cosmic rays or faulty NICs flip bits for you.

### Clock Drift

Simulated clocks can drift by up to 100ms (configurable) between nodes. This tests anything that depends on time agreement: lease expiration, distributed consensus, TTL handling, and cache invalidation. Clock drift is subtle because the code often works correctly with small drift and fails catastrophically when drift exceeds a threshold.

### Network Partitions

Moonpool supports three partition strategies:

| Strategy | Behavior | Tests |
|----------|----------|-------|
| Random | Random IP pairs partitioned | General chaos |
| UniformSize | Partition of random size (1 to n-1 nodes) | Various quorum scenarios |
| IsolateSingle | One node isolated from all others | Common production failure |

Partitions have configurable probability and duration. They can be programmatic (via `FaultContext::partition`) or automatic (via `partition_probability` in the chaos config).

### Connect Failures

Connection establishment can fail in two modes, following FoundationDB's `SIM_CONNECT_ERROR_MODE`:

- **AlwaysFail**: Every buggified connect attempt returns `ConnectionRefused`
- **Probabilistic**: 50% fail with `ConnectionRefused`, 50% hang forever (never complete)

The hanging mode is particularly nasty. Code that does not implement connect timeouts will block forever, which is exactly the kind of bug simulation should find.

## Graceful vs Abort Disconnect

When a connection closes, moonpool models two distinct TCP behaviors:

**Graceful close** implements TCP half-close semantics. The closing side marks its send direction as closed and schedules a `FinDelivery` event that arrives after all in-flight data has been delivered. The remote side continues reading buffered data normally and sees EOF only after the FIN arrives. This models a clean `shutdown(SHUT_WR)` followed by `close()`.

**Abort close** immediately terminates both directions. No FIN, no buffer drain. The remote side gets a connection reset error on its next read or write. This models a crashed process or a force-killed connection.

The distinction matters because many protocols depend on reading remaining data after the peer signals shutdown. HTTP/1.1 relies on this for chunked transfer encoding. gRPC uses it for trailing metadata. If your simulation only models abort closes, you will miss bugs in graceful shutdown handling.

## The Swizzling Insight

One finding from FoundationDB's simulation work deserves special mention: **restoring network connections in reverse order of disconnection finds more bugs than restoring in forward order**. This is called swizzling. As Will Wilson described it: "for reasons that we totally don't understand, this is better at finding bugs than normal clogging."

Why does this work? Forward restoration tests the easy case: the first connection dropped is the first restored, so recovery happens in the order the system expects. Reverse restoration forces the system to handle partial recovery where the most recently dropped connection comes back first. This creates asymmetric states that exercise recovery logic in ways no developer would think to test manually.

This is the kind of insight that only falls out of running thousands of simulations. No one sat down and reasoned that reverse-order restoration would find more bugs. The simulator tried both and the data spoke for itself.

## Configuration in Practice

For maximum chaos testing, use `NetworkConfiguration::random_for_seed()`. This randomizes all parameters based on the simulation seed, so different seeds test different network conditions:

```rust
let network_config = NetworkConfiguration::random_for_seed();
```

For fast unit tests where network chaos would just slow things down, use `NetworkConfiguration::fast_local()`:

```rust
let network_config = NetworkConfiguration::fast_local();
// Minimal latencies, all chaos disabled
```

For targeted testing of specific fault types, start with defaults and override:

```rust
let mut config = NetworkConfiguration::default();
config.chaos.partition_probability = 0.05;
config.chaos.partition_strategy = PartitionStrategy::IsolateSingle;
// Everything else at defaults
```

## Swarm Testing: Less Is More

There is a subtle trap hiding inside `random_for_seed()`. It sets **every** fault family to a random non-zero probability. Clogging is a little bit on, partitions are a little bit on, bit flips are a little bit on, all at once, on every seed. That sounds thorough. It is actually the opposite.

The problem is called **passive suppression**, and it comes straight from the "Swarm Testing" paper by Groce and colleagues (ISSTA 2012), popularized by Will Wilson's Antithesis talks. When every feature is always slightly active, the features crowd each other out. To find a clogging bug you often need clogging cranked hard, for a sustained stretch, with nothing else interfering. If partitions keep tearing down the connection you were trying to clog, you never drive clogging deep enough to hit its bug. The undirected default explores a narrow band in the middle of the configuration space and never visits the extremes where bugs actually live. In the paper, undirected swarm testing found **42% more distinct compiler crashes** than a heavily hand-tuned default.

The fix is counterintuitive: for each run, turn **off** a random subset of fault families entirely. Each seed enables roughly half the families and fully disables the rest. One seed is clogging-only. Another is partition-plus-bitflip. Another is the all-off no-fault baseline. Across many seeds you cover the single-family extremes that the all-on config can never reach.

```rust
SimulationBuilder::new()
    .swarm() // each seed enables a random subset of fault families
    // ...
```

`.swarm()` builds on `random_for_seed()`, then masks each of the seven network fault families to off with 50% probability: clog, partition, bit-flip, random-close, connect-failure, clock-drift, and buggified-delay. The all-off subset is allowed on purpose, because a pure no-fault run is a useful, valid config.

The interesting engineering detail is **where the randomness comes from**. Swarm decisions must not perturb the in-run randomness, or they would shift every fault's timing and break fork-explorer replay. So the subset is drawn from a separate `CONFIG_RNG` stream, seeded from the iteration seed but salted to decorrelate it from the main `SIM_RNG`. The config RNG never advances the in-run call counter. Same seed, same subset, every time, with zero effect on the simulation that follows.

You already met a tiny version of this idea earlier in the chapter. Connect failures have always had a one-in-three chance of being fully disabled per seed. Swarm testing simply generalizes that one disabled branch to every fault family at once.
