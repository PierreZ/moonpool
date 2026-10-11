# Workload: Your Test Driver

<!-- toc -->

A `Workload` exercises the system under test and judges its correctness. Processes are the code you ship. Workloads are the code that finds your bugs.

## The Workload Trait

```rust,ignore
#[async_trait]
pub trait Workload: Send + Sync + 'static {
    fn name(&self) -> &str;

    async fn setup(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        Ok(())
    }
}
```

Four methods, two with defaults. The lifecycle is strict: `setup()`, then `run()`, then `check()`. Each phase has its own rules.

## The Three Phases

### Setup: Prepare the Ground

`setup()` runs **sequentially** across workloads. Workload A's setup completes before workload B's starts.

Use it to find your servers, open connections and build data structures. When `run()` starts, every workload is ready.

```rust,ignore
async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    let server = ctx
        .topology()
        .all_process_ips()
        .first()
        .cloned()
        .ok_or_else(|| SimulationError::InvalidState("no servers".into()))?;
    self.server = Some(server);
    Ok(())
}
```

Do not hold a connection across setup if chaos is on. The server can reboot before `run()` starts. Connect inside `run()` and reconnect on error.

### Run: Drive the System

`run()` is where the action is. All workloads run **concurrently**: several clients hitting the same servers, competing for resources, triggering races.

Send requests, observe responses, keep a reference model, and assert on every answer. The phase ends when every workload returns or when the runner cancels `ctx.shutdown()`.

```rust,ignore
async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    for _ in 0..self.operations {
        if ctx.shutdown().is_cancelled() {
            break;
        }
        match self.random_op(ctx.random()) {
            Op::Write { key, value } => {
                self.send_write(ctx, &key, &value).await?;
                self.model.insert(key, value);
            }
            Op::Read { key } => {
                let got = self.send_read(ctx, &key).await?;
                let expected = self.model.get(&key).cloned();
                assert_always!(got == expected, "read returns the last committed write");
            }
        }
    }
    Ok(())
}
```

### Check: Judge the Outcome

`check()` runs **sequentially** after every `run()` has finished and the event queue has drained. The system is quiescent. No message is in flight, no timer is pending.

Use it for final validation. Did the conservation law hold? Did every acknowledged write survive the reboots? The verdict counts: a `check()` that returns `Err` or panics fails the seed, exactly as a failing `run()` does, even when every `run()` returned `Ok`.

```rust,ignore
async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
    let total = self.model.values().sum::<i64>();
    assert_always!(total == self.deposited - self.withdrawn, "money is conserved");
    Ok(())
}
```

## Workloads Survive Everything

This is the difference from processes. When the simulation kills a server, your workload keeps running. When a connection breaks, your workload reconnects. When the network partitions, your workload sees the errors and adapts.

Your workload remembers the whole run, across every reboot. That is how you verify recovery: the workload sent a write before the crash, the server rebooted, and the workload reads the value back.

A workload spawns tasks too, through `ctx.task().spawn_task()`. Those tasks carry no process scope, so they live as long as the workload does. A panic in a detached task is recorded against the workload and fails the seed unless the handle is awaited and the error handled.

## The Context

Every phase receives a `SimContext`. The parts a workload uses:

| Call | What it gives |
|------|---------------|
| `ctx.my_ip()` | this workload's address, on `10.0.0.x` |
| `ctx.client_id()`, `ctx.client_count()` | this instance's index and the instance count of its builder entry |
| `ctx.providers()` | the `SimProviders` bundle, for code generic over `P: Providers` |
| `ctx.network()`, `ctx.time()`, `ctx.task()`, `ctx.random()`, `ctx.storage()` | the individual providers |
| `ctx.topology()` | `all_process_ips()`, `ips_in_group(name)`, `ips_tagged(key, value)`, locality |
| `ctx.peer(name)`, `ctx.peers()` | other workloads by name |
| `ctx.state()` | shared state: `publish(key, value)` and `get::<T>(key)`, read by invariants and other workloads |
| `ctx.shutdown()` | the cancellation token the runner fires at the end of the run phase |
| `ctx.observability()` | a `TraceQuery` over the captured timeline, for `check()` |
| `ctx.metrics_handle()` | every node's metrics sources, to read a server's counters in `check()` |
| `ctx.disk_counters(ip)` | the simulator's ground-truth counters for a process's disk |
| `ctx.system_samples(ip)` | the last samples that process was handed by its `SystemProvider` |
| `ctx.slowness(ip)` | the process's current gray-failure factors, `Slowness::HEALTHY` when none |

The last three let a workload compare what a process reports about itself against what the simulator knows. A process that says "my disk is 90% busy" can be checked against `ctx.disk_counters(ip)`. A workload that sees a slow answer can ask `ctx.slowness(ip)` whether the machine is in a gray failure, and assert that a healthy one answers fast. [CPU Time and Gray Failures](../part3-building/26-cpu-and-gray-failure.md) builds on this.

## The Operation Alphabet

Strong workloads define an **operation alphabet**: the set of actions they can take. Deposit, withdraw, read, transfer, small delay. Each has a weight:

```rust,ignore
fn random_op(&self, random: &SimRandomProvider) -> Op {
    match random.random_range(0..100) {
        0..30 => Op::Deposit { .. },
        30..50 => Op::Withdraw { .. },
        50..70 => Op::Read { .. },
        70..90 => Op::Transfer { .. },
        _ => Op::SmallDelay,
    }
}
```

The weights matter. Too many reads and you never test write conflicts. Too many writes and you never test read-after-crash. Cover normal operations, adversarial inputs, and delays that let background work run.

Then let the seeds vary the alphabet. With `SimulationBuilder::swarm_operations()`, each seed enables a random subset of up to 256 operation ids, and `swarm_op_enabled(op_id)` tells the workload which ones:

```rust,ignore
let enabled: Vec<Op> = ALPHABET
    .iter()
    .copied()
    .filter(|op| swarm_op_enabled(op.id()))
    .collect();
```

Why? Some bugs only appear when an operation is **absent**. A compaction that is never triggered, a read that never races a write. A single workload with a swarmed alphabet finds those across seeds. Many separate workloads, one per scenario, do not. Prefer **one workload, many behaviours**. [Swarm Testing](../part3-building/28-swarm-testing.md) has the evidence, and [Designing Workloads That Find Bugs](../part3-building/19-designing-workloads.md) has the patterns.

## Multiple Instances

One client is rarely enough. `workloads()` builds several from a factory:

```rust,ignore
SimulationBuilder::new()
    .workloads(WorkloadCount::Fixed(3), |i| Box::new(ClientWorkload::new(i)))
```

Each instance gets its own `client_id()` and its own IP. `WorkloadCount::Random(1..6)` draws a different count per seed from the simulation stream, so it stays deterministic per seed. The factory receives the index, and the workload is responsible for a unique `name()`, such as `format!("client-{i}")`.

Factory workloads are also what `check_determinism()` and exploration need: an instance workload would carry its state from one timeline into the next.

## What a Good Workload Looks Like

1. Define an operation alphabet with weights, and swarm it.
2. Keep a reference model of the expected state.
3. Assert on every response with `assert_always!`.
4. Mark interesting paths with `assert_sometimes!`, so the run stops only when they fired.
5. Validate the final state in `check()` against the model.
6. Publish facts through `ctx.state()` and plain `tracing` events, so invariants can check cross-workload properties.

[Your First Simulation](../part3-building/01-first-simulation.md) builds one from scratch.
