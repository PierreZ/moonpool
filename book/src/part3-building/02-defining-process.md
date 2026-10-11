# Defining a Process

<!-- toc -->

Our key-value server is a Process. It listens for connections, answers get and set requests, and respects the shutdown token. Everything it does goes through providers, never through raw tokio calls.

## The Process Trait

The trait has two methods. `name()` identifies the process type. `run()` is the main loop, called on every boot.

```rust
#[async_trait]
pub trait Process: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
}
```

`run()` returns when the process exits on its own, or never returns because the simulator cancelled it. Both are normal. The trait is `Send + Sync + 'static` and uses plain `#[async_trait]`, so the process can share state through `Arc<RwLock<..>>` like any production server.

## The Process Struct

A Process is created from a factory on every boot, so the struct starts empty.

```rust
use async_trait::async_trait;
use moonpool_sim::{Process, SimContext, SimulationResult};

struct KvServer;
```

No fields. Each boot gets a fresh `KvServer`. Everything it accumulates lives until the next crash. A server that must survive reboots writes through `ctx.storage()`, and [Storage Faults](./11-storage-faults.md) shows what that costs.

## Implementing run()

The main loop binds a listener, then races `accept()` against the shutdown token.

```rust
use std::collections::BTreeMap;

use moonpool_sim::{NetworkProvider, TcpListenerTrait};

#[async_trait]
impl Process for KvServer {
    fn name(&self) -> &str {
        "kv"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        // In memory on purpose: a crash loses it, and the workload's
        // reference model must cope with that.
        let mut store: BTreeMap<String, Vec<u8>> = BTreeMap::new();

        loop {
            let accepted = moonpool_sim::select! {
                biased;
                accepted = listener.accept() => accepted,
                () = ctx.shutdown().cancelled() => return Ok(()),
            };
            if let Ok((stream, _addr)) = accepted {
                handle_connection(stream, &mut store).await;
            }
        }
    }
}
```

**Binding the listener.** `ctx.network().bind(ctx.my_ip())` registers this process's IP for incoming connections. No real port opens. The `NetworkProvider` and `TcpListenerTrait` traits must be in scope for `bind()` and `accept()`.

**The store.** A `BTreeMap` on the stack. When the process crashes, the map is gone. The ordered map also keeps iteration deterministic, which `HashMap` does not.

**The select.** `moonpool_sim::select!` is the deterministic twin of `tokio::select!`. With `biased;` it polls the branches in order and draws no randomness. Without `biased;` the branch offset comes from the seed, so the same seed still replays the same choice. `tokio::select!` is forbidden in simulated code: its branch offset comes from entropy the seed does not control.

**The shutdown token.** During a graceful reboot the simulator cancels `ctx.shutdown()` and waits a grace period. The `return Ok(())` branch lets the process exit cleanly. During a crash reboot the simulator aborts the task at once, so this branch never runs.

## Handling Requests

The connection handler parses a tiny wire protocol: `G<key>` for get and `S<key_len><key><value>` for set. A production system keeps its own framing or hands the stream to hyper, as [The hyper Stack](../part4-integration/08-hyper-stack.md) shows. A raw protocol shows the provider boundary directly.

```rust
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_sim::network::sim::SimTcpStream;

async fn handle_connection(mut stream: SimTcpStream, store: &mut BTreeMap<String, Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return, // closed, or a fault closed it for us
            Ok(n) => n,
        };
        let request = &buf[..n];
        let response = match request[0] {
            b'G' => {
                let key = String::from_utf8_lossy(&request[1..]);
                store.get(key.as_ref()).cloned().unwrap_or_default()
            }
            b'S' => {
                let key_len = usize::from(request[1]);
                let key = String::from_utf8_lossy(&request[2..2 + key_len]).to_string();
                let value = request[2 + key_len..].to_vec();
                store.insert(key, value.clone());
                value
            }
            _ => Vec::new(),
        };
        let _ = stream.write_all(&response).await;
    }
}
```

`SimTcpStream` implements `futures::io::AsyncRead` and `AsyncWrite`. The reads and writes are buffer-driven: a read completes on the current poll when bytes are waiting, and otherwise parks until a `Delivery` event lands them. Under network chaos a read can return an error or zero bytes at any point. The handler treats both as the end of the connection and returns.

## Registering with the Builder

`.processes()` takes a count and a factory. The factory runs on every boot.

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
```

This creates three servers at `10.0.1.1`, `10.0.1.2` and `10.0.1.3`. Each runs independently. Each can be rebooted independently.

A range makes the cluster size part of the seed:

```rust
.processes(3..=7, || Box::new(KvServer))
```

Every seed draws a count between 3 and 7 from the simulation stream. The same seed draws the same count. One `.processes()` call is one **group**, named after `name()`. A system with several roles registers one group per role, and [Process Groups, Tags and Topology](./27-process-groups.md) covers how they share the address plan.

## What a Process Knows About Itself

`ctx.my_ip()` is its address. `ctx.client_id()` is its index within its group and `ctx.client_count()` is the group's size, so three servers number themselves 0, 1 and 2. `ctx.topology()` answers the rest: its tags, its group, the other process IPs. A process that must crash itself at an exact point of its own protocol calls `ctx.crash_self(RebootKind::Crash, None)`, which [Correlated Outages and Self-Crashes](./30-outages.md) describes.

## What the Shutdown Token Gives You

Checking `ctx.shutdown()` is optional. If the process ignores it, a graceful reboot still works: the simulator force-kills the task when the grace period ends. Checking it tests a different code path from a hard crash. The process can:

- Finish in-flight requests.
- Flush write buffers.
- Close connections so peers see EOF instead of a reset.

[Attrition: Process Reboots](./09-attrition.md) covers the three reboot kinds and the grace period.

## Key Takeaways

Every Process follows the same pattern:

1. Bind a listener with `ctx.network()`.
2. Race `accept()` against `ctx.shutdown()` with `moonpool_sim::select!`.
3. Use `ctx.time()` for every sleep and timeout.
4. Keep state in memory and expect to lose it, or write it through `ctx.storage()`.
5. Return `Ok(())` on shutdown.

The factory produces a blank instance. The simulator manages the lifecycle. Your job is the server logic.
