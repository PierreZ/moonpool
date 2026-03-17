# Process: Your Server

<!-- toc -->

A Process represents the system under test. It is the code you would ship to production, running inside the simulation where the framework controls time, network, and failure.

## The Process Trait

The trait is minimal by design:

```rust
#[async_trait(?Send)]
pub trait Process: 'static {
    fn name(&self) -> &str;
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
}
```

Two methods. `name()` identifies this process type for reporting. `run()` is where your server logic lives. The `?Send` bound exists because moonpool runs on a single thread, so nothing needs to be `Send`.

When `run()` returns `Ok(())`, the process has exited voluntarily. When the simulation kills the process, the future is cancelled and `run()` never returns at all.

## The Factory Pattern

You never construct a Process once. You give the builder a **factory** that can produce fresh instances:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(MyServer::new()))
```

Why a factory? Because of reboots. When the simulation kills a process and restarts it, the framework calls your factory to get a brand new instance. This guarantees that each boot starts with a clean slate, just like restarting a real server.

The factory is called once per process per boot. Three processes with two reboots each means the factory runs nine times total.

## State and Reboots

This is the rule you must internalize: **all in-memory state is lost on reboot**.

If your Process has a `HashMap<String, Vec<u8>>` field tracking client sessions, that map is gone after a reboot. The new instance from the factory starts empty. Only data written to the simulated storage layer survives.

This matches reality. When a server process crashes and restarts, it does not magically recover its heap. It reads persistent state from disk and rebuilds from there.

## IP Addressing

Each process instance gets its own IP address in the `10.0.1.0/24` range:

```
Process 0 → 10.0.1.1
Process 1 → 10.0.1.2
Process 2 → 10.0.1.3
```

Workloads get IPs in the `10.0.0.0/24` range. This clean separation makes it easy to identify what is a server and what is a test driver when reading logs.

Your process accesses its IP through `ctx.my_ip()`. Other process IPs are available through `ctx.topology().all_process_ips()`.

## Tags for Role Assignment

Many distributed systems need nodes with different roles: leader and follower, primary and secondary, different data centers. Tags handle this:

```rust
SimulationBuilder::new()
    .processes(5, || Box::new(MyNode::new()))
    .tags(&[
        ("role", &["leader", "follower"]),
        ("dc", &["east", "west", "eu"]),
    ])
```

Tags distribute round-robin. With 5 processes and 2 role values, the assignment looks like:

| Process | role | dc |
|---------|---------|-------|
| 0 | leader | east |
| 1 | follower | west |
| 2 | leader | eu |
| 3 | follower | east |
| 4 | leader | west |

Inside your process, read tags from the context:

```rust
async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    let role = ctx.topology().my_tags().get("role");
    match role.as_deref() {
        Some("leader") => run_leader(ctx).await,
        Some("follower") => run_follower(ctx).await,
        _ => Ok(()),
    }
}
```

## Graceful vs Crash Reboots

Not all deaths are equal. Moonpool supports three reboot kinds:

**Graceful**: The simulation signals the shutdown token. Your process has a grace period to finish in-flight work, flush buffers, and close connections cleanly. If it does not exit in time, it gets force-killed anyway.

**Crash**: Instant death. The process task is cancelled immediately. All connections abort. No cleanup, no buffer drain. Peers see connection reset errors.

**CrashAndWipe**: Same as Crash but also wipes all persistent storage owned by that process. Simulates total disk failure or a fresh node joining the cluster. The wipe is immediate and scoped to the process's IP address, so other processes' storage is unaffected.

To handle graceful shutdown, check the cancellation token in your main loop:

```rust
async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    let listener = ctx.network().bind(ctx.my_ip()).await?;

    loop {
        if ctx.shutdown().is_cancelled() {
            break;
        }
        // Accept connections, handle requests...
    }
    Ok(())
}
```

You do not need to handle crash reboots. There is nothing to handle. The simulation cancels your future and moves on.

## A Concrete Example

Here is a simple echo server as a Process:

```rust
struct EchoServer;

#[async_trait(?Send)]
impl Process for EchoServer {
    fn name(&self) -> &str {
        "echo"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        loop {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            match ctx.time().timeout(
                Duration::from_millis(100),
                listener.accept()
            ).await {
                Ok(Ok((mut stream, _addr))) => {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 { break; }
                        let _ = stream.write_all(&buf[..n]).await;
                    }
                }
                _ => continue,
            }
        }
        Ok(())
    }
}
```

Notice the patterns: use `ctx.network()` for connections, `ctx.time()` for timeouts, `ctx.shutdown()` for graceful termination. Never call tokio directly.
