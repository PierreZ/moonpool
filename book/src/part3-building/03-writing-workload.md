# Writing a Workload

<!-- toc -->

The workload drives our key-value server and checks that it behaves. It sends requests, keeps a reference model of the expected state, and asserts on every answer while the run is still going.

## The Workload Trait

A workload has three lifecycle phases. The runner calls them in order.

```rust
#[async_trait]
pub trait Workload: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
}
```

- **setup** runs for every workload before any `run()` starts. Bind listeners, find peers, clear state.
- **run** is the main logic. All workloads run concurrently, under chaos when it is enabled.
- **check** runs after every `run()` has returned and the world has settled. An `Err` here fails the seed exactly like a failing `run()`.

Unlike a Process, a workload survives reboots. The servers die and come back. The test driver keeps running and keeps its model.

## The Workload Struct

```rust
use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_sim::{
    NetworkProvider, RandomProvider, SimContext, SimulationError, SimulationResult,
    TimeProvider, Workload, assert_always, assert_sometimes,
};

struct KvWorkload {
    /// Operations per seed. Finite, so the run ends.
    num_ops: usize,
    /// What the servers should contain, as far as this client knows.
    model: BTreeMap<String, Vec<u8>>,
    keys: Vec<String>,
}

impl KvWorkload {
    fn new(num_ops: usize) -> Self {
        Self {
            num_ops,
            model: BTreeMap::new(),
            keys: (0..16).map(|i| format!("k{i}")).collect(),
        }
    }
}
```

The `model` is the important field. Every write goes to the server and to the model. Every read is compared against the model.

## Setup: Finding the Servers

`ctx.topology()` knows every participant: which IPs are processes, which groups and tags they carry, which are other workloads.

```rust
#[async_trait]
impl Workload for KvWorkload {
    fn name(&self) -> &str {
        "kv-client"
    }

    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        if ctx.topology().all_process_ips().is_empty() {
            return Err(SimulationError::InvalidState("no kv servers".into()));
        }
        // A `.workload()` instance is reused across seeds: start each one clean.
        self.model.clear();
        Ok(())
    }
```

Three lookups cover most cases:

```rust
let servers = ctx.topology().all_process_ips();          // every process IP
let one = ctx.peer("kv");                                 // first process named "kv"
let leaders = ctx.topology().ips_tagged("role", "leader"); // by tag
let acceptors = ctx.topology().ips_in_group("acceptor");   // by group
```

## Run: The Operation Loop

The loop picks a server, picks an operation, and checks the answer. Every random choice comes from `ctx.random()`, so it replays with the seed.

```rust
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips().to_vec();

        for i in 0..self.num_ops {
            if ctx.shutdown().is_cancelled() {
                break;
            }
            let server = &servers[ctx.random().random_range(0..servers.len())];
            let key = self.keys[ctx.random().random_range(0..self.keys.len())].clone();

            match ctx.random().random_range(0..100) {
                0..40 => {
                    let value = format!("v{i}").into_bytes();
                    match self.send_set(ctx, server, &key, &value).await {
                        Ok(()) => {
                            self.model.insert(key, value);
                            assert_sometimes!(true, "kv: set succeeded");
                        }
                        // A fault is an outcome, not a failure. Reaching it is coverage.
                        Err(_) => assert_sometimes!(true, "kv: set failed under chaos"),
                    }
                }
                40..80 => {
                    if let Ok(value) = self.send_get(ctx, server, &key).await {
                        let expected = self.model.get(&key).cloned().unwrap_or_default();
                        assert_always!(
                            value == expected,
                            "kv: read matches the model",
                            { "key" => key, "server" => server }
                        );
                    }
                }
                _ => {
                    let _ = ctx.time().sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Ok(())
    }
```

Two assertion kinds work together here.

**`assert_always!`** guards an invariant. A get that disagrees with the model is a definite bug. The optional `{ key => value }` block prints with the seed when it fails, so the report names the key and the server.

**`assert_sometimes!`** marks a path that must fire at least once across the campaign. If `"kv: set failed under chaos"` never fires, the chaos is too weak. If `"kv: set succeeded"` never fires, the client cannot reach the servers at all.

The request helpers are ordinary provider code. Each one is bounded by a simulated timeout, because a connect to a server mid-reboot can hang until the network heals:

```rust
impl KvWorkload {
    async fn send_get(&self, ctx: &SimContext, server: &str, key: &str) -> SimulationResult<Vec<u8>> {
        let io = async {
            let mut stream = ctx.network().connect(server).await?;
            stream.write_all(format!("G{key}").as_bytes()).await?;
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await?;
            Ok::<_, std::io::Error>(buf[..n].to_vec())
        };
        match ctx.time().timeout(Duration::from_secs(2), io).await {
            Ok(result) => Ok(result?),
            Err(_) => Err(SimulationError::InvalidState("get timed out".into())),
        }
    }
}
```

`send_set` is the same shape with the `S` framing from [Defining a Process](./02-defining-process.md#handling-requests).

## Why run() Must Finish

A workload returns when it is done. The runner takes the first `Ok` from any workload as the signal to cancel `ctx.shutdown()` for every process and every other workload (except under `chaos_until_run_ends()`, where the runner waits for the last one). A workload that loops forever keeps the seed alive until the stall guards declare a deadlock. Count operations, or sleep for a fixed simulated duration, then return. [Running and Observing](./05-running.md#stall-detection) covers the guards.

A model that only knows what **this** client wrote is wrong under crashes: a `Crash` reboot loses the server's in-memory map, so a later get returns empty where the model expects a value. That is the first bug this workload finds, and it is a real one. The fix belongs in the server, through `ctx.storage()`, not in the assertion.

## Check: Final Validation

`check()` runs after the world is quiet. No messages in flight, no faults, processes aborted. (With `chaos_until_run_ends()` it runs on the live processes instead, after the cutoff: the quiet tail where the workload reads the final state back. See [Chaos in Moonpool](./07-chaos.md#chaos-for-the-whole-run).) It is the place for whole-run properties:

```rust
    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        assert_always!(
            self.model.len() <= self.keys.len(),
            "kv: model never holds more keys than the alphabet"
        );
        Ok(())
    }
}
```

`check()` runs inside the event loop, so network RPCs still work. A workload can also read the captured timeline here through `ctx.observability()`, which implements `TraceQuery`.

## Events for Invariants

Per-call checks live in the workload. Cross-workload properties live in invariants, which run after every simulation step and read **tracing events**. The workload emits plain production-style events:

```rust
// After a successful set: the same line an OpenTelemetry subscriber would see.
tracing::info!(key = %key, server = %server, "kv_set_committed");
```

An invariant registered on the builder scans `"kv_set_committed"` events with a cursor. [Events and Invariants](./17-events-and-invariants.md) covers the capture rule, `TraceQuery`, and the `Invariant` trait. For shared values that are not events, `ctx.state().publish("key", value)` and `ctx.state().get::<T>("key")` give workloads and fault injectors a typed blackboard.

## One Instance or Many

The builder offers three registrations:

- `.workload(KvWorkload::new(200))` keeps **one instance** across every seed. It gets `client_id` 0. Clear its state in `setup()`.
- `.workloads(WorkloadCount::Random(1..6), |i| Box::new(KvWorkload::new(200)))` builds **fresh instances** each seed, 1 to 5 of them, each with its own `client_id` and `client_count`.
- `.workload_factory(|| Box::new(KvWorkload::new(200)))` builds one fresh instance each seed.

Exploration and `check_determinism()` run a seed more than once and require the factory forms. [Designing Workloads That Find Bugs](./19-designing-workloads.md) explains why one workload with a wide operation alphabet beats several narrow ones.

## Patterns That Find Bugs

**Reference model.** Track expected state. Compare every answer. Any divergence is a bug.

**Weighted operation alphabet.** Mix writes, reads and pauses, and let [swarm testing](./19-designing-workloads.md#swarm-the-alphabet) vary the mix per seed.

**Both assertion kinds.** `assert_always!` for what must hold on every call. `assert_sometimes!` for what the campaign must reach.

**Errors are outcomes.** A network error under chaos is expected. Log it, count it, move on. The bug is a **wrong** answer, never an absent one.
