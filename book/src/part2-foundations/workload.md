# Workload: Your Test Driver

<!-- toc -->

A Workload exercises the system under test and validates its correctness. While Processes are the code you ship, Workloads are the code that finds your bugs.

## The Workload Trait

```rust
#[async_trait(?Send)]
pub trait Workload: 'static {
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

Four methods, two with defaults. The lifecycle follows a strict order: `setup()`, then `run()`, then `check()`. Each phase has different rules and a different purpose.

## The Three Phases

### Setup: Prepare the Ground

`setup()` runs **sequentially** across all workloads. Workload A's setup completes before workload B's setup starts. No concurrency, no surprises.

Use setup to establish connections, initialize state, prepare data structures. By the time `run()` starts, every workload should be ready to go.

```rust
async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    let server_ip = ctx.topology().all_process_ips()
        .first()
        .ok_or(SimulationError::InvalidState("no servers".into()))?;

    self.connection = Some(ctx.network().connect(server_ip).await?);
    Ok(())
}
```

### Run: Drive the System

`run()` is where the action happens. All workloads run **concurrently**. Multiple clients hammering the same servers, competing for resources, triggering race conditions.

This is where you send requests, observe responses, track expected state, and use assertions to flag anomalies. The run phase continues until all workloads return or the simulation shuts them down.

```rust
async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
    for _ in 0..self.num_operations {
        if ctx.shutdown().is_cancelled() {
            break;
        }

        let op = random_op(ctx.random(), &self.accounts);
        match op {
            Op::Write { key, value } => {
                self.send_write(&key, &value).await?;
                self.model.write(&key, &value);
            }
            Op::Read { key } => {
                let result = self.send_read(&key).await?;
                let expected = self.model.read(&key);
                assert_always!(
                    result == expected,
                    format!("read mismatch for key '{}'", key)
                );
            }
        }
    }
    Ok(())
}
```

### Check: Validate the Outcome

`check()` runs **sequentially** after all workloads finish and all pending events drain. The system is quiescent. No more messages in flight, no more timeouts pending.

Use check for final state validation. Did the conservation law hold? Are all balances non-negative? Did every committed write survive?

```rust
async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
    let total = self.model.total_balance();
    let expected = self.model.total_deposited - self.model.total_withdrawn;
    assert_always!(
        total == expected,
        format!("conservation law violated: {} != {}", total, expected)
    );
    Ok(())
}
```

## Workloads Survive Everything

This is the fundamental difference from Processes. When the simulation kills a server, your workload keeps running. When a connection breaks, your workload can reconnect. When the network partitions, your workload observes the failures and adapts.

Your workload tracks what happened across the entire simulation lifetime, including across process reboots. This is how you verify that a server recovers correctly after a crash: the workload sent a write before the crash, the server rebooted, and the workload reads the value back to check that it survived.

## The SimContext

Every lifecycle method receives a `SimContext` that gives workloads access to everything they need:

- **`ctx.my_ip()`** returns this workload's IP address
- **`ctx.topology()`** has process IPs, peer info, and tag registries
- **`ctx.network()`** provides simulated TCP connections
- **`ctx.time()`** provides simulated clocks and timeouts
- **`ctx.random()`** provides deterministic random numbers
- **`ctx.state()`** provides cross-workload shared state for invariants
- **`ctx.shutdown()`** provides the cancellation token

Use `ctx.topology().all_process_ips()` to find your servers. Use `ctx.peer("server")` if you know the name. Use `ctx.topology().ips_tagged("role", "leader")` if you need role-specific targeting.

## The Operation Alphabet

Strong workloads define an "operation alphabet": the set of actions they can perform. Deposits, withdrawals, reads, writes, delays. Each operation has a weight controlling how often it fires.

```rust
pub fn random_op(random: &SimRandomProvider, accounts: &[String]) -> Op {
    let roll = random.random_range(0..100);
    match roll {
        0..30  => Op::Deposit { ... },
        30..50 => Op::Withdraw { ... },
        50..70 => Op::Read { ... },
        70..90 => Op::Transfer { ... },
        _      => Op::SmallDelay,
    }
}
```

The weights matter. Too many reads and you never test write conflicts. Too many writes and you never test read-after-crash consistency. The alphabet should cover normal operations, adversarial inputs, and small delays that let background work complete.

## Multiple Workload Instances

Sometimes one client is not enough. Use `workloads()` to create multiple instances:

```rust
SimulationBuilder::new()
    .workloads(WorkloadCount::Fixed(3), |i| {
        Box::new(ClientWorkload::new(i))
    })
```

Each instance gets its own `client_id` (accessible via `ctx.client_id()`) and its own IP. Multiple clients hitting the same server concurrently is where the interesting bugs hide.

For variable topology testing, use `WorkloadCount::Random(1..6)` to spawn a different number of clients each iteration. The count is determined by the simulation RNG, so it stays deterministic per seed.

## What a Good Workload Looks Like

The best workloads follow a pattern:

1. **Define an operation alphabet** with weighted random selection
2. **Track a reference model** (expected state computed locally)
3. **Assert on every response** using `assert_always!` for invariants
4. **Use `assert_sometimes!`** for coverage of interesting paths
5. **Validate final state** in `check()` using the reference model
6. **Publish state** via `ctx.state()` for cross-workload invariant checking

The banking workload in moonpool's own test suite demonstrates all of these. We will build something similar in the next part.
