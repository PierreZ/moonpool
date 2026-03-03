# Defining a Process

<!-- toc -->

Our key-value server is a Process. It listens for connections, handles get/set requests, and respects shutdown signals. Everything it does goes through providers, never raw tokio calls.

## The Process Struct

Start with the struct. A Process is recreated from a factory on every boot, so the struct starts empty:

```rust
use async_trait::async_trait;
use moonpool_sim::{Process, SimContext, SimulationResult};

struct KvServer;
```

No fields. Each time the simulation boots this process, the factory returns a fresh `KvServer`. Any data it accumulates lives only until the next crash.

## Implementing the Trait

The trait has two methods: `name()` for identification and `run()` for the main logic.

```rust
#[async_trait(?Send)]
impl Process for KvServer {
    fn name(&self) -> &str {
        "kv"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let listener = ctx.network().bind(ctx.my_ip()).await?;

        let mut store: HashMap<String, Vec<u8>> = HashMap::new();

        loop {
            if ctx.shutdown().is_cancelled() {
                break;
            }

            let accept_result = ctx
                .time()
                .timeout(Duration::from_millis(100), listener.accept())
                .await;

            match accept_result {
                Ok(Ok((stream, _addr))) => {
                    handle_connection(stream, &mut store).await;
                }
                Ok(Err(e)) => {
                    tracing::warn!("accept error: {}", e);
                }
                Err(_) => {
                    // Timeout, loop back and check shutdown
                    continue;
                }
            }
        }
        Ok(())
    }
}
```

Walk through this line by line.

**Binding the listener**: `ctx.network().bind(ctx.my_ip())` creates a TCP listener on the process's assigned IP. In the simulated world, this registers the IP for incoming connections. No real ports are opened.

**The store**: A plain `HashMap` that lives on the stack. When this process crashes, the HashMap vanishes. When the factory creates a new instance, it starts with an empty map. This is the "all in-memory state is lost on reboot" principle in action.

**The main loop**: We loop forever, checking the shutdown token each iteration. `ctx.shutdown().is_cancelled()` returns `true` during graceful reboots, giving us a chance to break cleanly. For crash reboots, the framework cancels the entire future, so we never reach this check.

**Timeout on accept**: We use `ctx.time().timeout()` instead of `tokio::time::timeout()`. The simulated timer means the framework controls when the timeout fires, keeping everything deterministic.

## Handling Requests

The connection handler parses a simple wire protocol. For a real system, you would use moonpool-transport's RPC layer, but a raw protocol shows the fundamentals:

```rust
async fn handle_connection(
    mut stream: SimTcpStream,
    store: &mut HashMap<String, Vec<u8>>,
) {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break,       // Connection closed
            Ok(n) => n,
            Err(_) => break,      // Connection error
        };

        // Parse and handle the request
        let request = &buf[..n];
        let response = match request[0] {
            b'G' => {  // GET
                let key = String::from_utf8_lossy(&request[1..]);
                store.get(key.as_ref())
                    .cloned()
                    .unwrap_or_default()
            }
            b'S' => {  // SET
                // Format: S<key_len:u8><key><value>
                let key_len = request[1] as usize;
                let key = String::from_utf8_lossy(
                    &request[2..2 + key_len]
                ).to_string();
                let value = request[2 + key_len..].to_vec();
                store.insert(key, value.clone());
                value
            }
            _ => vec![],
        };

        let _ = stream.write_all(&response).await;
    }
}
```

## Registering with the Builder

The Process is registered through `.processes()` on the builder. The first argument is how many instances to run, the second is the factory:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(KvServer))
```

This creates 3 KvServer instances at IPs `10.0.1.1`, `10.0.1.2`, and `10.0.1.3`. Each one runs independently. Each one can be killed and restarted independently.

For variable cluster sizes, pass a range:

```rust
.processes(3..=7, || Box::new(KvServer))
```

Now each iteration randomly picks between 3 and 7 servers, deterministically based on the seed.

## What the Shutdown Token Gives You

Checking `ctx.shutdown()` is optional but valuable. During graceful reboots, the simulation cancels the token and gives a grace period. Your process can:

- Finish in-flight requests
- Flush write buffers
- Close connections cleanly so peers see EOF instead of reset errors

If you do not check the token, graceful reboots still work. The framework just force-cancels your future after the grace period expires. But checking gives your process a chance to exit cleanly, which tests a different code path than a hard crash.

## Key Takeaways

The pattern for every Process is the same:

1. Bind a listener using `ctx.network()`
2. Accept connections in a loop
3. Check `ctx.shutdown()` for graceful termination
4. Use `ctx.time()` for timeouts, never raw tokio
5. Keep your state in-memory, expect to lose it

The factory produces a blank instance. The simulation manages the lifecycle. Your job is to write the server logic and let the framework handle chaos.
