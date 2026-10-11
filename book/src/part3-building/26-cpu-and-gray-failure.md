# CPU Time and Gray Failures

<!-- toc -->

## Why a Poll Should Cost Something

Without a CPU model, a simulated task poll takes no time. Time moves only between events. A process that encodes a megabyte, folds a thousand records and sends the result does all of it at one instant. That is fine for most protocol bugs. It hides a whole family of others:

- a busy machine answers late, and a peer's timeout fires first,
- a burst of requests queues behind one core, and the tail latency grows,
- a busyness metric (`process_cpu`, `machine_busy`, `run_loop_busy`) that a load balancer or a leader election reads is always zero, so the code that reads it is never tested.

FoundationDB's simulator charges a small delay to a task as it runs (`sim2`'s `delay` before each actor resumes). Moonpool does the same, with one core per process.

## The Model

The CPU model is a per-seed chaos surface:

```rust
SimulationBuilder::new()
    .processes(5, || Box::new(Server::new()))
    .workload(Client)
    .enable_chaos([Chaos::Cpu(ChaosMode::Swarm)])
    .chaos_duration(Duration::from_secs(30))
```

`ChaosMode::Random` turns the model on for every seed, and `ChaosMode::Swarm` for about half of the seeds. A seed with the model off runs exactly as it would without the entry, apart from the one draw that decided it. When on, the seed draws a `CpuModel` (three draws):

| Field | Range | Meaning |
|-------|-------|---------|
| `poll_cost` | 0.5–5 µs | The cost of one poll of one task of a process |
| `sent_byte_cost_ps` | 0.5–5 ns per byte | The cost of each byte the process sends |
| `written_byte_cost_ps` | 0.5–5 ns per byte | The cost of each byte the process writes to its disk |

A low-level test sets a model directly with `SimWorld::set_cpu_model(Some(CpuModel::new(..)))` and reads the ground truth with `SimWorld::cpu_busy(ip)`.

How a poll pays:

1. Every simulated process (a task owner) has **one CPU slot**, free at some instant. The executor asks the world before it polls a process's task. If the slot is busy, the task is parked and a `CpuFree` event is scheduled for the instant the slot frees. A parked task is not runnable, and the stall detector counts the pending `CpuFree` as progress.
2. When the poll starts, the process's **local clock** is `now + poll_cost × slowness`. Every byte sent or written, and every explicit [`cpu_charge`](./08-buggify.md#cpu-charges), moves the local clock on. When the poll ends, the slot is free at the local clock.
3. While a poll runs, the process sees its local clock: `time().now()`, a timer, a storage submit, a sent chunk all happen after the work. A wake the poll causes directly (a read that frees the peer's window, a close) is delivered at the local clock too.
4. The network engine keeps one clock for both ends of a connection. The world shifts every network event a poll schedules by `local − now`, and a sent chunk leaves no earlier than its writer's local clock.

Tasks without an owner (the workload, a fault injector, the driver) are never gated and cost nothing. Network and storage completions are world events, not tasks, so they cost nothing either. Tasks a process spawns inherit its owner, so moonpool's own RPC tasks pay: they are shipped code, and they use the machine's CPU in production too.

A kill or a crash-restart drops the parked tasks of the process and frees its slot: the new incarnation starts on an idle core.

## Busyness Counters

With the model on, `SystemProvider::sample` reports real numbers. `process_cpu`, `machine_busy` and `run_loop_busy` are the time the core was busy up to the sample's instant, never past it, so `process_cpu ≤ at` always holds and two samples give a valid window. One process per machine and one run loop per process make the three equal. With the model off they stay at zero.

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

Each seed draws how many machines are slow (one to `max_slow`, among those the `victims` filter admits), which ones, and for each one a window of 30% to 100% of the chaos window and three factors:

| Kind | Default factor | What is multiplied |
|------|---------------|--------------------|
| CPU | ×10–×1000 | Every CPU cost of the process (only on seeds with the CPU model on, since a gray failure never turns the model on) |
| Disk | ×5–×100 | Every transfer and sync latency of the machine's disk |
| Network | ×2–×20 | The latency of every chunk sent to or from the machine (the larger factor of the two ends) |

`max_slow` is the budget: keep it at the number of machines your system tolerates losing (`f`), so a quorum can still form. `ChaosMode::Swarm` masks each kind per seed, so some seeds slow only the disk, some only the network, and so on. Recovery mode makes every machine healthy again, so the quiet tail runs at full speed. The reach gates `gray: cpu slow`, `gray: disk slow` and `gray: network slow` show which kinds a campaign reached.

A custom injector can do the same by hand with `FaultContext::set_slowness(ip, Slowness { cpu, disk, network })` and read it back with `FaultContext::slowness(ip)`.

## What the Gate Does Not See

The CPU gate stamps everything that goes through the world: timers, storage operations, network events and sent chunks, and the wakes they cause. A few paths reach another owner directly. Each is bounded by one poll's CPU time, or is not part of the system under test:

| Path | Why it is left alone |
|------|----------------------|
| Connection state the peer sees at once (an abort, a read-window credit, a listener bound or unbound) | The state is shared, but the wake that follows is stamped. The peer can see the change at most one poll's CPU early. |
| `StateHandle` | A harness channel between processes and the workload, not a production path. |
| A `JoinHandle` completion, an in-process channel between two owners | User or harness code that shares memory across machines, which does not happen between real machines. moonpool-rpc keeps its channels per process. |
| Tracing events | Stamped with the scheduler's time, not the local clock. |
| Harness readers (`SimWorld::cpu_busy`, `disk_counters` from a workload) | Ground truth, read at the scheduler's time. |

If your harness shares state between processes through an `Arc<Mutex<..>>`, it bypasses the gate the same way: a process sees what another wrote at the scheduler's time, up to one poll before its local clock. Keep such boards for observation only.
