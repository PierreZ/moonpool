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
    scope: AttritionScope::PerProcess,
}
```

**`max_dead`** is the most important field. It caps the number of simultaneously dead processes. If you have a 3-node cluster with `max_dead: 1`, attrition will never kill a second node before the first has restarted. This ensures the system always has enough live nodes to remain operational (assuming your replication factor matches).

**`prob_graceful`, `prob_crash`, `prob_wipe`** are weights, not probabilities. They do not need to sum to 1.0. The attrition injector normalizes them internally and picks a reboot kind by weighted random selection. Setting `prob_wipe: 0.0` disables wipe reboots entirely.

**`recovery_delay_ms`** controls how long a dead process stays dead before restarting. The actual delay is drawn randomly from this range, so different seeds test different recovery timings. The default is 1 to 10 seconds of simulated time.

**`grace_period_ms`** controls how long a graceful shutdown has to complete. Again, drawn randomly from the range. The default is 2 to 5 seconds.

**`scope`** decides which failure domain each reboot targets. The default, `AttritionScope::PerProcess`, kills one random process at a time. `PerMachine` and `PerZone` kill **all collocated processes together**, which is the subject of the next section.

## Using Attrition

Attrition is configured on the simulation builder and requires a chaos duration:

```rust
use moonpool_sim::{Attrition, Chaos, ChaosMode};

SimulationBuilder::new()
    .processes(3, || Box::new(MyProcess::new()))
    .enable_chaos([Chaos::Attrition {
        config: Attrition {
            max_dead: 1,
            prob_graceful: 0.3,
            prob_crash: 0.5,
            prob_wipe: 0.2,
            recovery_delay_ms: None,  // use defaults
            grace_period_ms: None,    // use defaults
            scope: AttritionScope::PerProcess,
        },
        mode: ChaosMode::Random,
    }])
    .chaos_duration(Duration::from_secs(60))
    .workload(MyWorkload::new())
    .run()
    .await;
```

The `.chaos_duration()` call is required because attrition runs only during the chaos phase. After the chaos duration elapses, fault injectors stop and the system continues until all workloads complete. A settle phase then drains remaining events before checks run, surfacing cleanup bugs rather than hiding them behind an arbitrary timer.

`ChaosMode::Random` uses your configured weights as written every seed. Switch to `ChaosMode::Swarm` to swarm the reboot *regime* itself: each seed draws a random subset of the configuration, including the never-reboot case (which surfaces slow-leak and timer bugs that constant restarting hides) and single-mode cases like always-crash or graceful-only. Same reasoning as [swarming network faults](10-network-faults.md#swarm-testing-less-is-more).

## The max_dead Constraint

`max_dead` deserves special attention because it is the bridge between chaos and correctness. Without it, attrition could kill all your nodes simultaneously, which is technically chaos but not useful chaos. No distributed system survives simultaneous failure of all replicas.

Set `max_dead` to match your system's fault tolerance. A system with replication factor 3 can tolerate 1 failure, so `max_dead: 1`. A system that needs 3 of 5 nodes alive should use `max_dead: 2`. This ensures attrition tests failures your system **should** survive, not failures that are inherently unrecoverable.

## Failure Domains: Correlated Reboots

`max_dead: 1` protects you from killing two random nodes at once. But real outages are rarely random and rarely one node at a time. A rack loses power and takes ten machines with it. A datacenter link cuts and a whole region goes dark. A host reboots and every process pinned to it dies together. These are **correlated failures**, and they are exactly the failures that break quorum logic, because the nodes that die were never independent to begin with.

Flat IPs cannot express this. `10.0.1.{1..N}` is a list of peers with no notion of which ones share fate. So moonpool borrows FoundationDB's model: a cluster is a hierarchy of **Datacenter → Zone → Machine → Process**, and processes on the same machine fail together because they share a machine.

You describe that hierarchy with `.cluster()` instead of `.processes()`:

```rust
use moonpool_sim::{LocalityConfig, SimulationBuilder};

SimulationBuilder::new()
    .cluster(
        // 3 datacenters × 3 zones × 3 machines × 2 processes = 54 processes.
        // Ranges like 1..=3 are sampled per seed, so every seed runs a
        // different cluster shape, the way FoundationDB generates topology.
        LocalityConfig::new(3, 3, 3, 2),
        || Box::new(MyProcess::new()),
    )
    .workload(MyWorkload::new());
```

The topology, not a flat count, decides how many processes exist. Each one is assigned a globally unique datacenter, zone, and machine id (`dc1`, `dc1-z1`, `dc1-z1-m1`), and processes read their own placement plus query the cluster through their topology:

```rust
let me = ctx.topology().my_locality().expect("clustered process");
let machine_mates = ctx.topology().peers_on_my_machine();
let same_dc = ctx.topology().ips_in_domain(DomainLevel::Datacenter, me.datacenter());
```

Now `scope` earns its keep. With `AttritionScope::PerMachine`, attrition picks a machine instead of a process and reboots **every process on it in the same instant**. The two processes that share `dc1-z1-m1` die together and recover together, exactly as they would when their host kernel panics. `PerZone` does the same one level up.

```rust
.enable_chaos([Chaos::Attrition {
    config: Attrition {
        max_dead: 2,  // one machine's worth of processes
        prob_graceful: 0.3,
        prob_crash: 0.5,
        prob_wipe: 0.2,
        recovery_delay_ms: None,
        grace_period_ms: None,
        scope: AttritionScope::PerMachine,
    },
    mode: ChaosMode::Random,
}])
```

`max_dead` still counts dead **processes**, and a machine reboot is atomic against that budget: attrition only kills a machine when the whole group fits within the remaining budget. With two processes per machine, `max_dead: 2` lets exactly one machine be down at a time and never leaves a machine half-dead. Set it to a multiple of your machine size that matches how many machines your replication can lose.

For targeted correlated faults, `FaultContext` exposes the group reboots directly. `ctx.reboot_machine("dc1-z1-m1", RebootKind::Crash)` kills a named machine and returns the IPs it took down, and `ctx.reboot_domain(DomainLevel::Datacenter, "dc1", kind)` blacks out a whole datacenter. Combined with `ctx.ips_in_domain(...)`, the same domain ids drive network partitions across a zone or datacenter boundary, so you can model a region cut as cleanly as a host reboot.

## Custom Fault Injection

Attrition covers the common case of random process reboots. For more targeted fault injection, implement the `FaultInjector` trait:

```rust
#[async_trait]
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
