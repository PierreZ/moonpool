# Network Faults

<!-- toc -->

## TCP, Not Packets

Moonpool simulates network faults at the **TCP connection level**, not the individual packet level. This is a deliberate design choice, inherited from FoundationDB. In practice, distributed systems rarely deal with individual packets. They deal with connections: connections that drop, connections that stall, connections that report success on one side and failure on the other. These are the faults that matter for application correctness.

Packet-level simulation (what TigerBeetle does) is useful for testing network stacks themselves. But for application-level distributed systems, connection-level faults exercise the code paths that actually fail in production: reconnection logic, request retries, leader election on disconnect, and state reconciliation after a partition.

## The Fault Catalog

Moonpool's `ChaosConfiguration` controls a wide range of network faults. Each fault is independently configurable and randomized per seed when using `NetworkConfiguration::random_for_seed()`.

### Latency Injection

Network timing uses configurable latency distributions. Bind, connect, and
accept are genuinely asynchronous: each allocates an operation,
schedules its completion, registers the caller's waker, and remains pending
until that exact operation becomes ready. Dropping one of these delayed futures
cancels its scheduled work. Reads wait for data delivery when the receive buffer
is empty. Writes accept available send-buffer capacity and schedule later byte
delivery, applying write and link latency on the delivery path.

This distinction is observable. A timeout can beat bind, connect, or accept,
and dropping the losing branch does not leave a stale completion behind. The
simulator still picks every delay deterministically from the configured
distribution, so the same seed reproduces the same race.

For tail latency testing, moonpool supports **bimodal latency distribution**, following FoundationDB's `halfLatency()` pattern. In bimodal mode, 99.9% of operations use normal latency, but 0.1% experience latencies multiplied by 5x to 20x. This is how real networks behave: most requests are fast, but a small fraction hit GC pauses, cross-datacenter hops, or congestion.

Re-sampling latency per operation has a blind spot: no single connection ever stays slow. Real clusters break differently. One machine sits behind a degraded switch port and every message to it lags, run after run, while the rest of the fleet is healthy. That **stably-slow peer** is exactly what stalls a quorum or reorders a consensus round, and uniform per-operation jitter never produces it. So moonpool borrows FoundationDB's `SimClogging` trick: set `max_pair_latency` to a range and each ordered IP pair draws **one** fixed latency at first contact, then carries it for the whole run. Off by default (`ZERO..ZERO`), it adds nothing. Turn it on and some pairs are permanently sluggish while others are quick, which is how you find the bug where the slow replica is the one holding the lease.

There is a second blind spot, and it is not a fault at all. A message between two processes on the same machine and a message between Paris and Montreal are both "one network operation" to the simulator, drawn from the same range. Real deployments do not work that way: loopback is tens of microseconds, a rack hop is hundreds, a cross-region hop is tens of milliseconds. If a replication protocol only ever sees uniform links, its cross-datacenter behavior is untested. `NetworkConfiguration::link_latency` fixes that. Give it a `LinkLatencyConfig` and each locality distance gets its own distribution:

```rust
SimulationBuilder::new()
    .cluster(LocalityConfig::new(2, 2, 2, 1), || Box::new(MyNode::new()))
    // Same machine, same zone, same datacenter, cross datacenter.
    .link_latency(LinkLatencyConfig::default())
```

The engine classifies every IP pair through the [`.cluster()`](09-attrition.md#failure-domains-correlated-reboots) topology, samples the matching distribution once at first contact, and keeps that value for the whole run, in the same per-pair budget as `max_pair_latency` (when both are on, the two samples are summed). A pair where either side has no locality, a workload client for instance, gets nothing extra, so plain `.processes()` runs are unaffected. FoundationDB does not model this at all, it runs a single global latency distribution, but a library that wants to find cross-region replication bugs needs the distance to be visible in the timing.

### Connection Drops

Random close injects spontaneous connection failures during I/O operations, at a configurable probability (default 0.001%). When triggered, 30% of closes are **explicit** (the caller gets an error) and 70% are **silent** (the connection just stops working). This ratio, taken from FoundationDB, tests both error-handling paths and timeout-based failure detection.

Fault decisions are sampled only when an operation can make progress. Re-polling a read with no buffered data or a write under backpressure returns `Poll::Pending` without consuming simulation randomness, so runtime-specific spurious polls cannot shift the seed's later fault and latency choices.

A cooldown period prevents cascading closes from overwhelming the system. The goal is to test recovery, not to make the system completely inoperable.

### Black Holes

A silent close still ends: the peer eventually reads EOF and learns something. A **black hole** never ends. When `black_hole_probability` fires, one direction of a connection (this side's sends, the peer's, or both, the same three-way draw random close makes) starts accepting every write and delivering nothing. The bytes are acknowledged into the sender's buffer and vanish when they would land, the peer's reads stay `Pending` with no data and no EOF, and a graceful close from the holed side never arrives either. Both ends see a connection that looks perfectly alive. That is what a peer whose kernel keeps acknowledging into a frozen application looks like, or a middlebox that dropped its connection state, and it is the fault that finds a request without a timeout: nothing errors, nothing closes, and only the caller's own deadline can notice. An application-level timeout, an HTTP/2 keep-alive ping, a heartbeat: whatever detects it in production is what has to detect it here.

```rust
let mut config = NetworkConfiguration::fast_local();
config.chaos.black_hole_probability = 0.0001; // per I/O, like random close
config.chaos.black_hole_cooldown = Duration::from_secs(5);
```

The coin is rolled on the same operations as random close, under the same progress rule, with its own cooldown, and it is recorded once as a `black_hole` fault event. Three properties keep it honest. A black hole is **permanent for the connection**: it never recovers, and the only way out is the one production has, a new connection. An **abort still crosses**: a process kill resets its connections and the peer finds out, as a kernel `RST` would once the host is back, so a hung peer is never mistaken for a dead one at the wrong time. And **recovery mode stops new black holes but keeps the ones in force**, like a closed connection; the quiet tail is where the reconnect has to happen. `SimWorld::black_hole_connection(id, hole_send, hole_recv)` is the scripted form for fault injectors, and the family is off by default, masked as `NetworkFault::BlackHole`.

### Clogging

Write clogging stalls data delivery on a connection for a random duration (100-300ms by default). This simulates network congestion, TCP backpressure, and flow control contention. Code that assumes writes complete promptly will fail under clogging.

### Partial Writes

Writes are truncated to a random length (0 to 1000 bytes by default), following FoundationDB's approach. This tests TCP fragmentation handling and message framing logic. If your wire protocol assumes that a single write delivers a complete message, partial writes will break that assumption immediately.

### Partial Reads

Reads are the mirror image: a read returns a random prefix of what is buffered (1 to 1000 bytes by default), and the rest stays buffered for the next read. FoundationDB's `Sim2Conn` does the same 50/50 split on the receiver. The one asymmetry with writes is that a partial read always delivers **at least one byte** when data is available, because a zero-byte read is how the stream signals end-of-file. This exercises the reassembly side of your wire protocol: code that calls `read` once and assumes it got a whole message will drop bytes the moment a short read splits a header or a length prefix.

### Bit Flips

Packet data is corrupted with random bit flips at low probability (0.01% by default). The number of flipped bits follows a power-law distribution between 1 and 32. This tests checksum validation and corruption detection. Without bit-flip injection, corruption bugs only surface in production when cosmic rays or faulty NICs flip bits for you.

### Clock Drift

Simulated clocks can drift by up to 100ms (configurable) between nodes. This tests anything that depends on time agreement: lease expiration, distributed consensus, TTL handling, and cache invalidation. Clock drift is subtle because the code often works correctly with small drift and fails catastrophically when drift exceeds a threshold.

### Buggified Sleep Delay

FoundationDB occasionally stretches a process timer with a power-law delay to
exercise timeouts and timing-dependent races. Moonpool applies the same fault to
`TimeProvider::sleep`. In a `SimulationBuilder` campaign it is active only during
the configured `chaos_duration`: setup is clean, and sleeps scheduled at or
after the chaos deadline receive exactly their requested duration. This makes
the post-chaos quiet tail a real recovery period, so a claim such as "the
cluster converges within five seconds after faults stop" is measuring the
cluster rather than a fault that silently continued running.

The gate is checked before either buggified-delay RNG draw. A configuration that
disables the fault, a `NetworkFaultMask` without `BuggifiedDelay`, and sleeps
outside the chaos window therefore leave the counted simulation RNG untouched.
Direct low-level `SimWorld` construction has no campaign phases and retains the
historical whole-run behavior, unless you call `SimWorld::enter_recovery_mode()`
yourself.

The same quiet-tail guarantee holds for every other network fault family, not
just this one: at the `chaos_duration` cutoff the runner calls
`SimWorld::enter_recovery_mode()`, which zeroes the whole
`ChaosConfiguration` fault surface and heals any partition in force. What it
does **not** do is repair damage: bytes already corrupted stay corrupted, a
connection the application already saw close stays closed, and a link that has
already sampled its permanent extra latency stays slow. See the
[configuration appendix](../appendix/03-configuration.md#chaos-duration-and-recovery-mode)
for the full boundary contract.

### Network Partitions

Moonpool supports seven partition strategies:

| Strategy | Behavior | Tests |
|----------|----------|-------|
| Random | Random IP pairs partitioned | General chaos |
| UniformSize | Partition of random size (1 to n-1 nodes) | Various quorum scenarios |
| IsolateSingle | One node isolated from all others | Common production failure |
| IsolateZone | Every process of one random zone cut from the rest | Rack or availability-zone loss |
| IsolateDatacenter | Every process of one random datacenter cut from the rest | Region loss, cross-datacenter replication |
| AsymmetricSend | One node's outgoing traffic blocked, incoming still flows | Half-reachable node, failure detectors |
| AsymmetricRecv | One node's incoming traffic blocked, outgoing still flows | The mirror case |

Partitions have configurable probability and duration. They can be programmatic (via `FaultContext::partition`) or automatic (via `partition_probability` in the chaos config).

At the lower-level `SimWorld` API, `partition_pair(from, to, duration)` adds one directed pair entry. Call it for both directions to create a bidirectional pair cut. A single `restore_partition(from, to)` deliberately removes both directed entries, preventing a half-healed pair during recovery. Send-wide and receive-wide partitions remain separate and expire through their targeted restore events.

A partition never punches a hole in an established connection. A queued send
whose direction is cut **stalls**: the bytes stay at the front of the send
buffer, the writer keeps seeing backpressure, and the stream resumes in order
the moment the partition heals, whether that is at its deadline or through an
early `restore_partition` / `FaultContext::heal_partition`. The peer therefore
either reads the original bytes in order, or sees the connection fail. It never
reads a later chunk in place of one that was silently dropped, which for a
framed protocol (h2 on the same connection) would corrupt the frame that follows.
This mirrors FoundationDB's `SimClogging`, where a clogged pair adds delay and
only an explicit disconnect fails the connection.

The same holds for bytes that are already **on the wire**. A chunk that left
the send buffer has a delivery time sampled from the write latency, and that
time was sampled before the partition existed; the partition still decides
its fate. Every byte moves through four places, in order:

```text
application write
    │  poll_write
local send queue          stalls under a cut
    │  ProcessSendBuffer samples latency
in flight                 frozen under a cut, re-timed by the heal
    │  Delivery event
peer receive buffer       the peer's now; nothing takes it back
    │  poll_read
application read
```

When a cut lands, every chunk (and any FIN) in flight on the cut direction is
**frozen** where it is. Its `Delivery` event still fires at the old time and
does nothing. When every partition blocking the direction has healed, each
frozen item is re-timed by exactly the time the direction spent cut, so a cut
of `D` delays every byte it caught by `D`, the stream keeps its order (a chunk
sent after the heal lands behind the last frozen one), and no randomness is
drawn: the latencies already sampled are reused. A response written at `t=0`
with `100ms` of latency does not slip through a partition that starts at
`t=50ms`; it lands at `100ms + (heal − 50ms)`. Directed, send-wide, and
receive-wide cuts all freeze the same way, an explicit heal and the
recovery-mode heal both thaw the same way, and a FIN is an item like any
other: the peer sees the last bytes and then EOF, never EOF first.

The first three strategies are IP-blind: they pick nodes out of a hat. `IsolateZone` and `IsolateDatacenter` read the [`.cluster()`](09-attrition.md#failure-domains-correlated-reboots) topology instead, so the cut lands exactly where a real one would, on the boundary that shares a switch or a region. Without a topology they fall back to `Random` selection, which keeps them safe to draw for any seed.

The asymmetric pair is worth dwelling on. A node whose sends are blocked still hears every heartbeat from the cluster, so it happily believes it is healthy while everyone else marks it dead. Systems that infer liveness from "I can see you" rather than "you can see me" split brains here. Both arms record their own fault events (`SendPartitionCreated`, `RecvPartitionCreated`) on the timeline, so an invariant can correlate application behavior with the exact one-way cut that caused it.

### Connect Failures

Connection establishment can fail in two modes, following FoundationDB's `SIM_CONNECT_ERROR_MODE`:

- **AlwaysFail**: Every buggified connect attempt returns `ConnectionRefused`
- **Probabilistic**: 50% fail with `ConnectionRefused`, 50% hang forever (never complete)

The hanging mode is particularly nasty. Code that does not implement connect timeouts will block forever, which is exactly the kind of bug simulation should find.

## Graceful vs Abort Disconnect

When a connection closes, moonpool models two distinct TCP behaviors:

**Graceful close** implements TCP half-close semantics. The closing side marks its send direction as closed and puts a FIN on the wire behind every byte still queued or in flight, so it lands after all of them and is held by a partition with them. The remote side continues reading buffered data normally and sees EOF only after the FIN arrives. This models a clean `shutdown(SHUT_WR)` followed by `close()`.

**Abort close** immediately terminates both directions. No FIN, no buffer drain: the send queue and the flight are gone. The remote side gets a connection reset error on its next read or write. This models a crashed process or a force-killed connection.

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

When a builder should keep its per-seed Random or Swarm profile but exclude one
environmental fault family, use a `NetworkFaultMask`. For example, this campaign
retains clogs, partitions, short reads and writes, closes, latency, and the other
sampled faults while disabling wire-data corruption:

```rust
SimulationBuilder::new()
    .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
    .network_fault_mask(
        NetworkFaultMask::all().without(NetworkFault::BitFlip),
    )
    .enable_exploration(exploration_config)
```

The builder samples the complete profile first, applies buggify knob changes,
then applies the mask immediately before creating `SimWorld`. Masking performs
no RNG draws. The sampled values and Swarm subset therefore stay identical to
an unmasked run, the in-run RNG call count does not move, and an exploration
recipe replays deterministically. With the default all-family mask, this step is
a no-op.

## Swarm Testing: Less Is More

There is a subtle trap hiding inside `random_for_seed()`. It sets **every** fault family to a random non-zero probability. Clogging is a little bit on, partitions are a little bit on, bit flips are a little bit on, all at once, on every seed. That sounds thorough. It is actually the opposite.

The problem is called **passive suppression**, and it comes straight from the "Swarm Testing" paper by Groce and colleagues (ISSTA 2012), popularized by Will Wilson's Antithesis talks. When every feature is always slightly active, the features crowd each other out. To find a clogging bug you often need clogging cranked hard, for a sustained stretch, with nothing else interfering. If partitions keep tearing down the connection you were trying to clog, you never drive clogging deep enough to hit its bug. The undirected default explores a narrow band in the middle of the configuration space and never visits the extremes where bugs actually live. In the paper, undirected swarm testing found **42% more distinct compiler crashes** than a heavily hand-tuned default.

The fix is counterintuitive: for each run, turn **off** a random subset of fault families entirely. Each seed enables roughly half the families and fully disables the rest. One seed is clogging-only. Another is partition-plus-bitflip. Another is the all-off no-fault baseline. Across many seeds you cover the single-family extremes that the all-on config can never reach.

```rust
SimulationBuilder::new()
    // each seed enables a random subset of network fault families
    .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
    // ...
```

`ChaosMode::Swarm` builds on `random_for_seed()`, then masks each of the eight network fault families to off with 50% probability: clog, partition, bit-flip, random-close, connect-failure, clock-drift, buggified-delay, and permanent pair latency. The all-off subset is allowed on purpose, because a pure no-fault run is a useful, valid config. The same `enable_chaos` call swarms storage faults (`Chaos::Storage`) and the attrition reboot regime (`Chaos::Attrition`) the same way.

The interesting engineering detail is **where the randomness comes from**: the same place as everything else. The subset is drawn from the simulation's one stream, seeded for the iteration, before the world is built and before any process runs, as a fixed number of draws per family (one coin each, consumed whether or not the family was on). Same seed, same subset, every time: a replay, fork-explorer recipes included, rebuilds the subset from the seed before the counted run starts. There is no separate configuration RNG: moonpool draws its own decisions from the stream it hands to the code under test.

You already met a tiny version of this idea earlier in the chapter. Connect failures have always had a one-in-three chance of being fully disabled per seed. Swarm testing simply generalizes that one disabled branch to every fault family at once.
