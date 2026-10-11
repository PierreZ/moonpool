# Network Faults

<!-- toc -->

## Connections, Not Packets

Moonpool injects network faults at the **TCP connection level**. FoundationDB made the same choice. A distributed system does not see packets. It sees connections that drop, stall, or report success on one side and failure on the other. Those are the faults that reach application code: reconnection, retries, leader election on disconnect, and reconciliation after a partition.

TigerBeetle simulates packets, which is the right tool for testing a network stack. For an application on top of TCP, connection faults reach the code paths that fail in production.

## Turning Faults On

Every network fault lives in `NetworkConfiguration`, under its `chaos` field (a `ChaosConfiguration`). The builder picks a configuration per seed from the `Chaos::Network` entry:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(Node::new()))
    .workload(Client)
    // Each seed enables a random subset of the nine fault families.
    .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::BuggifyKnobs])
    .chaos_duration(Duration::from_secs(30))
```

| Entry | Configuration the seed gets |
|-------|-----------------------------|
| none | `NetworkConfiguration::default()`: random close at 0.001%, clock drift up to 100 ms, buggified sleep delay, probabilistic connect failures. Clogs, partitions, bit flips, black holes and pair latency are off. |
| `Chaos::Network(ChaosMode::Random)` | `NetworkConfiguration::random_for_seed()`: every family on, with per-seed probabilities, durations and a per-seed `PartitionStrategy`. |
| `Chaos::Network(ChaosMode::Swarm)` | `swarm_for_seed()`: the Random profile, then each of the nine families is switched off with 50% probability. The all-off subset is allowed. |

`Chaos::BuggifyKnobs` composes on top. For a seed where a `buggify_knob!` site fires, one knob jumps to an extreme: 2 to 5 clogs per second, 0.5 to 2 partitions per second, random closes and black holes at 1% to 10%. A knob only spikes a family the seed already has. It never turns on a family the swarm turned off. See [Swarm Testing](28-swarm-testing.md) for why "a little of everything" finds fewer bugs than "a lot of a few".

`NetworkConfiguration::fast_local()` is the no-chaos profile for unit tests, with microsecond latencies and `ChaosConfiguration::disabled()`.

## The Nine Families

Each family is one `NetworkFault` variant, one bit of the swarm mask, and a few knobs on `ChaosConfiguration`.

| `NetworkFault` | Knobs | What it finds |
|----------------|-------|---------------|
| `Clog` | `clog_rate` (clogs per simulated second), `clog_duration` (100 to 300 ms) | Code that assumes a write lands promptly. One IP's sends, receives or both stall on every connection, then resume in order (FoundationDB's `RandomClogging`). |
| `Partition` | `partition_rate` (partitions per simulated second), `partition_duration`, `partition_strategy` | Split brain, quorum loss, failure detectors. See below. |
| `BitFlip` | `bit_flip_probability`, `bit_flip_min_bits`/`max_bits` (1 to 32), `bit_flip_cooldown` | Missing checksums. The flip count follows a power law. |
| `RandomClose` | `random_close_probability`, `random_close_cooldown`, `random_close_explicit_ratio` (30%) | Reconnection and redelivery. 30% of closes return an error, 70% go silent and end in EOF, FoundationDB's ratio. |
| `ConnectFailure` | `connect_failure_mode` (`Disabled`, `AlwaysFail`, `Probabilistic`), `connect_failure_probability` | Missing connect timeouts. In `Probabilistic` mode a buggified connect returns `ConnectionRefused`, and a small share of the others (one in ten) hangs forever. |
| `ClockDrift` | `clock_drift_enabled`, `clock_drift_max` (100 ms) | Leases, TTLs, anything that compares clocks across nodes. |
| `BuggifiedDelay` | `buggified_delay_enabled`, `buggified_delay_max`, `buggified_delay_probability` (25%) | Timeouts. `TimeProvider::sleep` stretches by a power-law delay, only inside the chaos window. |
| `PairLatency` | `max_pair_latency` (off at `ZERO..ZERO`) | A stably slow peer. Each ordered IP pair draws one extra latency at first contact and keeps it for the run (FoundationDB's `SimClogging`). |
| `BlackHole` | `black_hole_probability`, `black_hole_cooldown` | Requests without a deadline. See below. |

Bit flips are off in the default profile, because TCP delivers what was written: only a protocol that checksums its frames can tell a flip from data. FoundationDB flips bits only where `FlowTransport` verifies a packet checksum. If your protocol has no checksum (HTTP/2, gRPC), mask the family out.

Two shapes are always on and are not families: **short writes** and **short reads**. A write delivers a random prefix of up to `partial_write_max_bytes` (1000), and a read returns a random prefix of what is buffered, up to `partial_read_max_bytes`, always at least one byte, because a zero-byte read means EOF. Code that calls `read` once and expects a whole message breaks here first.

A fault coin is only rolled when an operation can make progress. A re-poll of a read with no data, or a write under backpressure, returns `Pending` and draws nothing, so runtime-specific spurious polls do not shift the seed.

## Partitions

A partition cuts one direction between two IPs. Random partitions arrive as a Poisson process in simulated time, at `partition_rate` per second. The count does not grow with traffic: a busy run and a quiet run see the same number. At most one random partition is in force at a time. The next one is drawn from the moment the current one heals, so every partition is followed by a healed gap (TigerBeetle's partition stability). Clogs arrive the same way, at `clog_rate` per second for the whole network, and may overlap as in FoundationDB.

`partition_strategy` decides who is cut when a partition arrives:

| Strategy | Cut |
|----------|-----|
| `Random` | Random IP pairs |
| `UniformSize` | A random group of 1 to n-1 nodes from the rest |
| `IsolateSingle` | One node from everyone |
| `IsolateZone` | Every process of one zone from the rest |
| `IsolateDatacenter` | Every process of one datacenter from the rest |
| `AsymmetricSend` | One node's outgoing traffic only |
| `AsymmetricRecv` | One node's incoming traffic only |

`IsolateZone` and `IsolateDatacenter` read the [`.cluster()`](27-process-groups.md) topology. Without one they fall back to `Random`, so every seed can draw them. The asymmetric pair finds the split brain where a node hears every heartbeat but nobody hears its answers. Each arm records its own fault event (`SendPartitionCreated`, `RecvPartitionCreated`) on the timeline.

A partition **stalls** bytes. It never drops them. A chunk queued or in flight on the cut direction freezes where it is and is re-timed by the length of the cut when the partition heals, with no new random draw. The peer reads the original bytes in order, or sees the connection fail. It never reads a later chunk in place of a missing one, which would corrupt the next frame of h2 on the same connection. A FIN is a chunk like any other, so the peer sees the last bytes, then EOF, never EOF first.

A scripted injector cuts and heals by hand:

```rust
// Inside a FaultInjector: isolate the leader for two seconds.
ctx.partition(&leader, &follower)?;
if ctx.time().sleep(Duration::from_secs(2)).await.is_err() {
    return Ok(()); // the run is shutting down
}
ctx.heal_partition(&leader, &follower)?;
```

`FaultContext::partition` cuts both directions. `FaultContext::heal_partition` and `is_partitioned` complete the set. On `SimWorld`, `partition_pair(from, to, duration)` adds one directed entry, `restore_partition(from, to)` removes both, and `partition_send_from(ip, duration)` / `partition_recv_to(ip, duration)` are the one-way cuts.

## Black Holes

A silent close ends in EOF. A black hole never ends. When the coin fires, one direction (this side's sends, the peer's, or both) starts delivering nothing. Bytes are accepted into the send window and vanish. The peer's reads stay `Pending` with no data and no EOF. Both ends look alive. Only the caller's own deadline can notice, which is the point: a request without a timeout hangs here forever.

```rust
let mut config = NetworkConfiguration::fast_local();
config.chaos.black_hole_probability = 0.0001; // per I/O, like random close
config.chaos.black_hole_cooldown = Duration::from_secs(5);
```

A black hole is permanent for its connection. The only way out is a new connection. A process kill still resets its connections, so a hung peer is never mistaken for a dead one. `SimWorld::black_hole_connection(id, hole_send, hole_recv)` is the scripted form.

## Closing a Connection

Moonpool models three ways a connection ends, and your protocol must survive all three:

- **Graceful close**: the closer's FIN goes behind every queued byte. The peer reads everything, then EOF. Dropping a drained stream does this.
- **Write shutdown**: `AsyncWrite::close` sends the FIN, further writes fail with `BrokenPipe`, and the read half stays open. The reply to an EOF-delimited request still arrives.
- **Abort**: no FIN, no drain. The peer gets a reset on its next read or write. A process crash does this, and so does dropping a stream with unread bytes.

HTTP/1.1 chunked bodies and gRPC trailers both depend on reading after the peer's shutdown. A simulator with only aborts never finds those bugs.

## Deployment Shape

Three builder settings describe the network rather than break it. They apply on every seed and draw no randomness:

| Builder method | Default | Effect |
|----------------|---------|--------|
| `.link_latency(LinkLatencyConfig)` | none | Per-distance one-way latency: `same_machine` 10 to 50 µs, `same_zone` 100 to 500 µs, `same_datacenter` 0.5 to 2 ms, `cross_datacenter` 20 to 80 ms. Each IP pair samples once at first contact. Needs a `.cluster()` topology. |
| `.tcp_send_window_bytes(n)` | 64 KiB | The end-to-end byte window per direction. Credit returns only when the peer's application reads. A small window makes a slow reader back its writer up early. |
| `.accept_backlog_capacity(n)` | 128 | Unaccepted connections per listener. A full backlog parks `connect()` until an `accept()` returns. |

Flow control is entropy-neutral: a `Pending` on a full window and the wake after a read draw nothing. Congestion control and retransmission are out of scope. The model is a reliable ordered byte stream with a bounded number of bytes outstanding.

## Masks and Recovery

A `NetworkFaultMask` removes a family after the seed's profile is sampled. It draws nothing, so the seed's other values, its RNG call count and any exploration recipe stay the same:

```rust
SimulationBuilder::new()
    .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
    // gRPC over h2 has no frame checksum: a flipped bit is just data.
    .network_fault_mask(NetworkFaultMask::all().without(NetworkFault::BitFlip))
```

A mask only removes. `NetworkFaultMask::all()` is the default and changes nothing.

At the `chaos_duration` cutoff the runner calls `SimWorld::enter_recovery_mode()`. It zeroes every fault probability, heals every partition and makes every machine healthy. It does not repair damage: flipped bytes stay flipped, a closed connection stays closed, a black hole stays open, and a pair latency already sampled stays slow. The quiet tail is where the reconnect has to happen. The [Configuration Reference](../appendix/03-configuration.md) lists the full boundary contract, and the [Fault Reference](../appendix/04-fault-reference.md) lists every knob.

One FoundationDB result is worth keeping in mind when you write a [custom injector](31-custom-faults.md): **restoring connections in reverse order of disconnection finds more bugs than restoring them in order**. Will Wilson calls it swizzling, and nobody reasoned their way to it. The simulator tried both and the data decided.
