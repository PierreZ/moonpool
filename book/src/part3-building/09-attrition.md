# Attrition: Process Reboots

<!-- toc -->

## Why Processes Need to Die

A distributed system that only works when all nodes are healthy is not a distributed system. It is a single point of failure with extra network hops. Real clusters lose nodes constantly: rolling deployments restart processes, kernel panics crash them, power failures wipe their storage. Your system must handle all of these, and it must handle them while continuing to serve requests.

Attrition is moonpool's built-in mechanism for automatically killing and restarting server processes during simulation. It runs during the chaos phase, picks random processes, kills them in various ways, waits a recovery delay, and restarts them. The goal is to continuously verify that your system can tolerate node failures without manual intervention.

## Three Kinds of Reboot

Moonpool provides three reboot types, each modeling a different real-world failure:

### Graceful

The controlled shutdown. A cancellation token fires, giving the process a grace period to drain buffers, close connections cleanly, and flush pending writes. If the process does not exit within the grace period, it gets force-killed. After shutdown, connections deliver remaining buffered data (FIN semantics), then the process restarts with fresh state.

This models rolling deployments, planned maintenance, and well-behaved process managers. The process has a chance to clean up, but that chance is time-bounded.

### Crash

The sudden death. The process task is immediately cancelled. All connections abort with no buffer drain. Peers see connection reset errors. Any in-memory state is lost. The process restarts after a recovery delay.

This models kernel panics, OOM kills, and hardware failures. There is no warning and no cleanup. Code that assumes a graceful shutdown will always happen gets a rude surprise.

### CrashAndWipe

The worst case. Same as Crash, but all persistent storage for the process is also deleted. The process restarts as if it were a brand new node joining the cluster for the first time.

This models total disk failures, accidental data deletion, or replacing a failed machine with a fresh one. The wipe is scoped to the crashed process's IP address, so other processes' storage is unaffected. Systems that rely on durable state for recovery must handle the case where that state is gone.

## The Attrition Configuration

Attrition is configured through the `Attrition` struct:

```rust
Attrition {
    max_dead: 1,
    prob_graceful: 0.3,
    prob_crash: 0.5,
    prob_wipe: 0.2,
    recovery_delay_ms: Some(1000..10000),
    grace_period_ms: Some(2000..5000),
}
```

**`max_dead`** is the most important field. It caps the number of simultaneously dead processes. If you have a 3-node cluster with `max_dead: 1`, attrition will never kill a second node before the first has restarted. This ensures the system always has enough live nodes to remain operational (assuming your replication factor matches).

**`prob_graceful`, `prob_crash`, `prob_wipe`** are weights, not probabilities. They do not need to sum to 1.0. The attrition injector normalizes them internally and picks a reboot kind by weighted random selection. Setting `prob_wipe: 0.0` disables wipe reboots entirely.

**`recovery_delay_ms`** controls how long a dead process stays dead before restarting. The actual delay is drawn randomly from this range, so different seeds test different recovery timings. The default is 1 to 10 seconds of simulated time.

**`grace_period_ms`** controls how long a graceful shutdown has to complete. Again, drawn randomly from the range. The default is 2 to 5 seconds.

## Using Attrition

Attrition is configured on the simulation builder and requires a chaos duration:

```rust
SimulationBuilder::new()
    .processes(3, || Box::new(MyProcess::new()))
    .attrition(Attrition {
        max_dead: 1,
        prob_graceful: 0.3,
        prob_crash: 0.5,
        prob_wipe: 0.2,
        recovery_delay_ms: None,  // use defaults
        grace_period_ms: None,    // use defaults
    })
    .chaos_duration(Duration::from_secs(60))
    .workload(MyWorkload::new())
    .run()
    .await;
```

The `.chaos_duration()` call is required because attrition runs only during the chaos phase. After the chaos duration elapses, fault injectors stop and the system continues until all workloads complete. A settle phase then drains remaining events before checks run, surfacing cleanup bugs rather than hiding them behind an arbitrary timer.

## The max_dead Constraint

`max_dead` deserves special attention because it is the bridge between chaos and correctness. Without it, attrition could kill all your nodes simultaneously, which is technically chaos but not useful chaos. No distributed system survives simultaneous failure of all replicas.

Set `max_dead` to match your system's fault tolerance. A system with replication factor 3 can tolerate 1 failure, so `max_dead: 1`. A system that needs 3 of 5 nodes alive should use `max_dead: 2`. This ensures attrition tests failures your system **should** survive, not failures that are inherently unrecoverable.

## Custom Fault Injection

Attrition covers the common case of random process reboots. For more targeted fault injection, implement the `FaultInjector` trait:

```rust
#[async_trait(?Send)]
impl FaultInjector for RollingRestart {
    fn name(&self) -> &str { "rolling_restart" }

    async fn inject(&mut self, ctx: &FaultContext) -> SimulationResult<()> {
        // Restart each process in order, waiting for recovery between each
        for ip in ctx.process_ips().to_vec() {
            ctx.reboot(&ip, RebootKind::Graceful)?;
            ctx.time().sleep(Duration::from_secs(15)).await
                .map_err(|e| SimulationError::InvalidState(e.to_string()))?;

            if ctx.chaos_shutdown().is_cancelled() {
                break;
            }
        }
        Ok(())
    }
}
```

The `FaultContext` provides access to process reboots, network partitions, and tag-based targeting. You can combine built-in attrition with custom fault injectors by registering both on the builder.
