# CPU Time and Gray Failures

<!-- toc -->

## A Poll Should Cost Something

Without a CPU model, a task poll takes no simulated time. Time moves only between events. A process that encodes a megabyte, folds a thousand records and sends the result does all of it at one instant. That hides a whole family of bugs:

- a busy machine answers late, and a peer's timeout fires first,
- a burst of requests queues behind one core, and the tail latency grows,
- a busyness metric that a load balancer or a leader election reads is always zero, so the code that reads it is never tested.

FoundationDB's simulator charges a small delay to a task as it runs. Moonpool does the same, with **one core per process**.

## The CPU Gate

The CPU model is a per-seed chaos surface:

```rust
SimulationBuilder::new()
    .processes(5, || Box::new(Server::new()))
    .workload(Client)
    .enable_chaos([Chaos::Cpu(ChaosMode::Swarm)])
    .chaos_duration(Duration::from_secs(30))
```

`ChaosMode::Random` turns the model on for every seed. `ChaosMode::Swarm` turns it on for half of them. A seed with the model off runs exactly as it would without the entry, apart from the draw that decided it. A seed with the model on draws a `CpuModel`:

| Field | Range | Meaning |
|-------|-------|---------|
| `poll_cost` | 0.5 to 5 µs | One poll of one task of a process |
| `sent_byte_cost_ps` | 0.5 to 5 ns per byte | Each byte the process sends |
| `written_byte_cost_ps` | 0.5 to 5 ns per byte | Each byte the process writes to its disk |

A low-level test sets a model with `SimWorld::set_cpu_model(Some(CpuModel::new(..)))` and reads the ground truth with `SimWorld::cpu_busy(ip)`. A fault injector reads the seed's model with `FaultContext::cpu_model()`.

How a poll pays:

1. Every process has **one CPU slot**, free at some instant. Before the executor polls a process's task, it asks the world. If the slot is busy, the task is parked and a `CpuFree` event is scheduled for the instant the slot frees. The stall detector counts that event as progress.
2. When the poll starts, the process's **local clock** is `now + poll_cost × slowness`. Every byte sent or written, and every explicit `cpu_charge`, moves the local clock on. When the poll ends, the slot is free at the local clock.
3. While a poll runs, the process sees its local clock. `time().now()`, a timer, a storage submit and a sent chunk all happen after the work.
4. The network engine keeps one clock for both ends of a connection. The world shifts every network event a poll schedules by `local − now`, and a sent chunk leaves no earlier than its writer's local clock.

Tasks without an owner (the workload, a fault injector, the driver) are never gated and cost nothing. Network and storage completions are world events, not tasks, so they cost nothing either. Tasks a process spawns inherit its owner, so moonpool's own RPC tasks pay: they are shipped code, and they use the machine's CPU in production too.

A kill or a crash drops the parked tasks of the process and frees its slot. The new incarnation starts on an idle core.

## Charging for Real Work

A poll costs a small base amount. The explicit charges carry the signal. `moonpool_buggify::cpu_charge` lets shipped code say it just did a known amount of work:

```rust
fn fold_batch(records: &[Vec<u8>]) {
    // ... the real work ...
    moonpool_buggify::cpu_charge(Duration::from_nanos(200) * records.len() as u32);
}
```

Put a charge only on a hot path a profile shows: an encode or decode of a batch, a checksum, a fold. Like `hint!`, a charge is **inert** outside a simulation. No sink is installed, so the call costs one thread-local read and does nothing. The simulator installs its sink only while its CPU model is on, and then the calling process's clock moves forward by the cost times its slowness. The same code runs in production and in simulation. See [Same Code in Production and Simulation](../part2-foundations/12-same-code.md).

## Reading the Counters

`SystemProvider` is the sixth provider. It reads **cumulative** CPU and disk counters the way the kernel exposes them (`getrusage`, `/proc/stat`, `/proc/diskstats`) and does no arithmetic. A caller takes two `SystemSample`s and divides the differences by the elapsed time, as FoundationDB's `getSystemStatistics` does:

```rust
// Inside a process: sample every 100 ms, as a load reporter would.
if let Ok(sample) = ctx.providers().system().sample("") {
    samples.push(sample);
}
```

A `SystemSample` carries `at`, `process_cpu`, `cores`, `machine_busy`, `machine_total`, `run_loop_busy` and `disk: Option<DiskCounters>`. In production, `TokioSystemProvider` reads the kernel. In simulation, with the model on, `process_cpu`, `machine_busy` and `run_loop_busy` are the time the core was busy up to the sample's instant, never past it, so `process_cpu ≤ at` always holds. One process per machine and one run loop per process make the three equal. With the model off they stay at zero. The disk counters are the simulated disk's own: `io_ticks` is the time with at least one operation in flight, so a stall or throttle episode makes it rise. FoundationDB turns its system monitor off in simulation. Moonpool does not, so the code that reads the counters runs under simulation too.

A workload checks what a process reported against the samples it was really given with `SimContext::system_samples(ip)` and `SimContext::disk_counters(ip)`.

## Gray Failures

A crash is easy to see: the peer times out and moves on. A slow machine is harder. It answers, late. Its heartbeats arrive, a leader on it keeps its lease, and it drags the cluster down. `Chaos::GrayFailure` slows one or more machines for a long window and keeps them alive:

```rust
SimulationBuilder::new()
    .processes(5, || Box::new(Acceptor::new()))
    .enable_chaos([
        Chaos::Cpu(ChaosMode::Swarm),
        Chaos::GrayFailure {
            config: GrayFailure::new(2).victims(AttritionVictims::group("acceptor")),
            mode: ChaosMode::Swarm,
        },
    ])
    .chaos_duration(Duration::from_secs(30))
```

When the chaos window opens, the seed draws how many machines are slow (one to `max_slow`, among the processes `victims` admits), which ones, and for each one a window and three factors:

| `Slowness` field | Default range | What is multiplied |
|------------------|---------------|--------------------|
| `cpu` | 10 to 1000 | Every CPU cost of the process. Only on seeds with the CPU model on: a gray failure never turns the model on. |
| `disk` | 5 to 100 | Every transfer and sync latency of the machine's disk |
| `network` | 2 to 20 | The latency of every chunk sent to or from the machine (the larger factor of the two ends) |

Each window is 30% to 100% of the chaos window, placed at a random start inside it. `GrayFailure::new(max_slow)` sets the defaults, and the `window`, `cpu`, `disk`, `network` and `kinds` fields are public. `max_slow` is the budget: keep it at the number of machines your system tolerates losing, so a quorum can still form. `ChaosMode::Swarm` masks each kind per seed, so some seeds slow only the disk and some only the network. The reach gates `gray: cpu slow`, `gray: disk slow` and `gray: network slow` show which kinds a campaign reached. A `GrayFailure` that names an unregistered group fails the run with `InvalidConfiguration`.

Recovery mode makes every machine healthy again, so the quiet tail runs at full speed.

A custom injector does the same by hand with `FaultContext::set_slowness(ip, Slowness { cpu, disk, network })` and reads it back with `FaultContext::slowness(ip)`. `Slowness::HEALTHY` is all ones. A workload reads a machine's factors with `SimContext::slowness(ip)`, for example to check that a slow process reports itself as slow:

```rust
// Inside a workload: a slow acceptor must not hold the lease.
if !ctx.slowness(acceptor_ip).is_healthy() {
    assert_sometimes!(leader != acceptor_ip, "a slow machine gave up the lease");
}
```

## What the Gate Does Not See

The gate stamps everything that goes through the world: timers, storage operations, network events, sent chunks and the wakes they cause. A few paths reach another owner directly. Each is bounded by one poll's CPU time, or is not part of the system under test:

| Path | Why it is left alone |
|------|----------------------|
| Connection state the peer sees at once (an abort, a read-window credit, a listener bound or unbound) | The state is shared, but the wake that follows is stamped. The peer can see the change at most one poll's CPU early. |
| `StateHandle` | A harness channel between processes and the workload, not a production path. |
| A `JoinHandle` completion, an in-process channel between two owners | Harness code that shares memory across machines. moonpool-rpc keeps its channels per process. |
| Tracing events | Stamped with the scheduler's time, not the local clock. |
| Harness readers (`SimWorld::cpu_busy`, `disk_counters` from a workload) | Ground truth, read at the scheduler's time. |

If your harness shares state between processes through an `Arc<Mutex<..>>`, it bypasses the gate the same way. Keep such boards for observation only.
