# Process: Your Server

<!-- toc -->

A `Process` is the system under test. It is the code you ship, running inside a simulation where the framework controls time, the network, the disk and failure.

## The Process Trait

The trait is minimal by design:

```rust,ignore
#[async_trait]
pub trait Process: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
}
```

Two methods. `name()` identifies the process type, and it names the process **group** the builder registers. `run()` holds your server logic. Every `Process` is `Send + Sync + 'static`, so it composes with `Arc<RwLock<…>>` and `DashMap` like ordinary tokio code. The runtime is still one thread.

When `run()` returns `Ok(())` or an `Err`, the process has exited on its own. When the simulation kills the process, the future is cancelled and `run()` never returns. So is every task the process spawned through `ctx.task().spawn_task()`, however deep: a crash ends the whole process, not only its root future, so a background worker never survives to run beside the rebooted process. When `run()` **panics**, the seed fails. The panic is caught and recorded against the process's IP, and the iteration is reported as failed even if every workload returned `Ok`.

## The Factory

You never construct a `Process` once. You give the builder a **factory**:

```rust,ignore
SimulationBuilder::new()
    .processes(3, || Box::new(MyServer::new()))
```

Why a factory? Reboots. When the simulation kills a process and restarts it, the runner calls the factory for a new instance. Each boot starts from a clean slate, as a restarted server does. Three processes with two reboots each means nine factory calls.

The count can be a range. `.processes(3..=7, …)` draws a count per seed, so cluster size is one more thing the seeds explore.

## State and Reboots

This is the rule to internalize: **all in-memory state is lost on reboot.**

If your process keeps a `BTreeMap` of client sessions in a field, that map is gone after a reboot. The new instance starts empty. Only data written to the simulated disk survives, and of that, only what was synced. A write that completed but was not synced resolves by crash physics: kept, lost, or torn, sector by sector. [Storage Faults](../part3-building/11-storage-faults.md) describes the crash model.

This matches reality. A crashed server does not recover its heap. It reads persistent state from disk and rebuilds.

To carry a fact across boots **for the test's sake**, use `ctx.state()`, the shared state handle. It belongs to the run, not to the process, so a boot counter published there is visible to the next boot and to every workload and invariant. Use it for test bookkeeping only. Shipped code must not depend on it.

## The Context

`run()` receives a `SimContext`. The parts a process uses:

| Call | What it gives |
|------|---------------|
| `ctx.my_ip()` | this process's address, as a `&str` |
| `ctx.providers()` | the `SimProviders` bundle, for code generic over `P: Providers` |
| `ctx.network()`, `ctx.time()`, `ctx.task()`, `ctx.random()`, `ctx.storage()` | the individual providers |
| `ctx.providers().system()` | the system provider: CPU and disk counters |
| `ctx.topology()` | process IPs, groups, tags, locality |
| `ctx.client_id()`, `ctx.client_count()` | this process's index within its group, and the group size |
| `ctx.shutdown()` | the cancellation token a graceful reboot fires |
| `ctx.state()` | the run's shared state |
| `ctx.metrics::<S>()` | this node's metrics source, when a `metrics_factory` is set |
| `ctx.crash_self(kind, delay)`, `ctx.self_crash()` | crash this process now, from inside it |

The providers are fresh each boot. Bind listeners, open files and connect to peers inside `run()`, not in the factory.

## Tags for Roles

Many systems need nodes with different roles: leader and follower, primary and secondary, several data centres. Tags handle this:

```rust,ignore
SimulationBuilder::new()
    .processes(5, || Box::new(MyNode::new()))
    .tags(&[
        ("role", &["leader", "follower"]),
        ("dc", &["east", "west", "eu"]),
    ])?
```

`tags()` applies to the last registered group and returns a `Result`, because it is an error to call it before `processes()`. Tags distribute round-robin:

| Process | role | dc |
|---------|---------|-------|
| 0 | leader | east |
| 1 | follower | west |
| 2 | leader | eu |
| 3 | follower | east |
| 4 | leader | west |

Read your tags from the topology:

```rust,ignore
async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    match ctx.topology().my_tags().get("role") {
        Some("leader") => run_leader(ctx).await,
        Some("follower") => run_follower(ctx).await,
        _ => Ok(()),
    }
}
```

`ctx.topology().ips_tagged("role", "leader")` finds the others. For a second role with its own process type, register a second group instead. [Process Groups, Tags and Topology](../part3-building/27-process-groups.md) covers both.

## Three Kinds of Reboot

`RebootKind` has three variants:

**Graceful.** The runner cancels `ctx.shutdown()`. The process has a grace period to finish in-flight work and close connections. Send buffers drain, so peers see a clean FIN. If the process does not return in time, it is force-killed.

**Crash.** Instant death. The task is cancelled at the crash instant. Connections reset, so peers see `ConnectionReset`. Unsynced storage data may be lost.

**CrashAndWipe.** A crash that also deletes every file the process owns. This simulates a lost disk or a fresh node joining the cluster. The wipe is scoped to the process's IP.

Handle a graceful shutdown by racing your work against the token. The `biased;` form polls the branches in order and draws nothing, which is right when one branch is a guard:

```rust,ignore
loop {
    let result = moonpool_sim::select! {
        biased;
        r = listener.accept() => r,
        () = ctx.shutdown().cancelled() => return Ok(()),
    };
    // ...
}
```

You do not handle a crash. There is nothing to handle. The simulation cancels your future and moves on.

## Crashing Yourself

Sometimes a crash must land at an exact point of your own protocol: between a write and its sync, in the middle of an append. `ctx.crash_self(RebootKind::Crash, Some(Duration::from_millis(10)))` schedules the same force kill a fault injector would, one scheduler tick from now, and restarts the process after the delay. With `None` the process stays down until an injector restarts it. `ctx.self_crash()` returns a `SelfCrash` handle that is `Clone + Send + Sync`, for code that holds no context. A graceful kind is refused: a process shuts down gracefully by returning. This is a test instrument. Shipped code names the moment with `hint!` instead and lets the simulator decide, as [Same Code in Production and Simulation](./12-same-code.md) explains.

## A Concrete Example

An echo server as a `Process`, from moonpool's own reboot tests:

```rust,ignore
use futures::io::{AsyncReadExt, AsyncWriteExt};

struct EchoProcess;

#[async_trait]
impl Process for EchoProcess {
    fn name(&self) -> &'static str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        loop {
            let result = moonpool_sim::select! {
                biased;
                r = listener.accept() => r,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };

            match result {
                Ok((mut stream, _peer)) => {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = stream.read(&mut buf).await {
                        if n > 0 {
                            let _ = stream.write_all(&buf[..n]).await;
                        }
                    }
                }
                // Under chaos an accept can fail; the server keeps serving.
                Err(e) => tracing::debug!("accept error: {e}"),
            }
        }
    }
}
```

The patterns: `ctx.network()` for sockets, `ctx.shutdown()` for a graceful stop, `moonpool_sim::select!` for the race. Never call tokio directly. In a real service the body of `run()` would call a function generic over `P: Providers` with `ctx.providers()`, and `main` would call the same function with `TokioProviders`.
