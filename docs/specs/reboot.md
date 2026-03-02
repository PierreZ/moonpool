# Spec: Process + Workload Separation & Reboot Support

## Problem

Moonpool-sim conflates the "system under test" and the "test driver" into a single
`Workload` trait. This prevents modeling realistic distributed system scenarios
(Paxos consensus, Orleans actor clusters) where server nodes crash and restart
while test-driver clients keep running and observe behavior.

Inspired by FoundationDB's simulation framework, which separates `fdbd` server
processes from test workloads (`tester.actor.cpp`).

## Design

### Core Separation

**Process** — the system under test. Runs on a server node. Can be killed and
restarted (rebooted). A fresh instance is created from a factory on every boot.
State only persists through storage (not in memory).

**Workload** — the test driver. Survives server reboots. Created once per
iteration. Drives requests to the cluster and validates correctness.

### Traits

```rust
/// System under test. Factory called on every boot (fresh instance).
/// Process reads tags/index from SimContext to determine its role.
#[async_trait(?Send)]
pub trait Process: 'static {
    /// Name of this process type for reporting.
    fn name(&self) -> &str;

    /// Run the process. Called on each boot (first boot and every reboot).
    /// The SimContext has fresh providers each boot.
    /// Returns when the process exits, or gets cancelled on reboot.
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
}

/// Test driver. Survives server reboots. Unchanged from current trait
/// except semantics: workload start() triggers simulation end on completion.
#[async_trait(?Send)]
pub trait Workload: 'static {
    fn name(&self) -> &str;
    async fn setup(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()>;
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> { Ok(()) }
}
```

### Reboot Kinds

```rust
pub enum RebootKind {
    /// Signal shutdown token, wait grace period, drain send buffers, then restart.
    Graceful,
    /// Instant kill: task cancelled, all connections abort immediately.
    Crash,
    /// Instant kill + wipe all storage for this process (future work: storage not yet scoped per IP).
    CrashAndWipe,
}
```

**Graceful**: The process's `ctx.shutdown()` token fires. Process has a grace
period to finish up. If it doesn't exit in time, the task is force-cancelled.
Send buffers drain during the grace period (FIN delivery).

**Crash**: Task cancelled immediately. All connections involving the process IP
are aborted (existing `close_connection_abort` logic). No buffer drain.

**CrashAndWipe**: Same as Crash. Storage wipe deferred to future work (storage
not yet scoped per IP).

### Recovery

After a process is killed, a `ProcessRestart` event is scheduled in the event
queue at `now + recovery_delay`. The recovery delay is drawn from a seeded random
range (deterministic per seed).

When the `ProcessRestart` event fires, the orchestrator calls the process factory
again to create a fresh instance, builds a new `SimContext` with fresh providers,
and spawns a new `run()` task.

### Tags & Topology

Processes get tags at registration time. Tags are distributed round-robin:

```rust
.processes(5, || PaxosNode::new())
    .tags(&[
        ("dc", &["east", "west", "eu"]),   // east, west, eu, east, west
        ("rack", &["r1", "r2"]),            // r1, r2, r1, r2, r1
    ])
```

Each tag dimension distributes independently. Framework-agnostic: users define
whatever topology makes sense for their system.

Topology exposes tag queries:
- `ctx.topology().all_process_ips()` — all server IPs
- `ctx.topology().ips_tagged("dc", "east")` — IPs matching a tag
- `ctx.topology().tags_for(ip)` — all tags on a specific IP

### Fault Targeting

FaultContext gains reboot methods:
- `ctx.reboot(ip, kind)` — reboot specific process
- `ctx.reboot_random(kind)` — reboot random alive server process
- `ctx.reboot_tagged("dc", "east", kind)` — reboot all matching tag

### Built-in Attrition

Default chaos for common cases. Power users write custom `FaultInjector` impls.

```rust
pub struct Attrition {
    /// Maximum number of simultaneously dead processes.
    pub max_dead: usize,
    /// Probability of graceful reboot (0.0..1.0).
    pub prob_graceful: f64,
    /// Probability of crash reboot (0.0..1.0).
    pub prob_crash: f64,
    /// Probability of crash + wipe reboot (0.0..1.0).
    pub prob_wipe: f64,
}
```

Probabilities are weights (normalized internally). Attrition loops with seeded
random delays, picks random alive processes, respects `max_dead`.

### Builder API

```rust
SimulationBuilder::new()
    // Server processes: factory called on every boot
    .processes(3..=5, || MyNode::new())      // seeded range or fixed count
        .tags(&[
            ("dc", &["east", "west", "eu"]),
        ])
    // Test driver: survives reboots
    .workload(TestDriver::new())
    // Built-in attrition (chaos phase only)
    .attrition(Attrition {
        max_dead: 1,
        prob_graceful: 0.3,
        prob_crash: 0.5,
        prob_wipe: 0.2,
    })
    // OR custom fault injector
    .fault(MyCustomInjector::new())
    // Cross-system invariants (always run, even during recovery)
    .invariant(MyInvariant::new())
    .run()
```

Process count from a range is resolved once per iteration using the seeded RNG.

### IP Addressing

- Processes: `10.0.1.{1..N}` (server subnet)
- Workloads: `10.0.0.{1..M}` (client subnet, same as today)

### Orchestrator Lifecycle

```
Per iteration:
  1. Resolve process count (fixed or seeded range)
  2. Assign IPs, distribute tags (round-robin)
  3. Boot all processes (call factory, create SimContext, spawn run())
  4. Run workload.setup()
  5. Spawn workload.run()
  6. Event loop:
     - step() one simulation event
     - Check invariants (always, even during recovery with dead processes)
     - If ProcessRestart event: call factory, create fresh SimContext, spawn run()
     - If attrition enabled + chaos phase: periodically kill random process
     - Collect finished handles
     - Deadlock detection
     - yield_now()
  7. Workload start() returns → trigger shutdown
  8. Kill all running processes
  9. Drain remaining events
  10. Run workload.check()
```

### Phase Interaction

- Reboots and attrition are only active during the chaos phase
- Recovery phase: stop attrition, let all recovering processes restart, then continue
- Chaos→recovery transition heals partitions AND stops killing

### Client Impact

When a process is killed, clients (workloads) with pending connections get
connection-reset errors. The workload must handle reconnection and retries
itself. This is realistic — production clients must handle server failures.

### All-Dead Scenario

If all processes die simultaneously (fault injector deliberately kills everything),
the simulation keeps running. Recovery events fire, processes restart, client
workloads get connection errors until processes come back. Useful for testing
total cluster failure + recovery.

### Process Role via Context

Single Process type per simulation. The process reads its tags/index from
SimContext to decide its role (like FDB where all nodes run the same `fdbd`
binary). No need for multiple Process types.

```rust
impl Process for MyNode {
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let role = ctx.topology().my_tags().get("role").unwrap();
        match role {
            "acceptor" => run_acceptor(ctx).await,
            "learner" => run_learner(ctx).await,
            _ => unreachable!(),
        }
    }
}
```

## Example: Paxos Consensus

```rust
struct PaxosNode;

impl Process for PaxosNode {
    fn name(&self) -> &str { "paxos" }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let log = ctx.storage().open("paxos-log").await?;
        let transport = PaxosTransport::bind(ctx).await?;
        paxos_main_loop(log, transport, ctx.shutdown()).await
    }
}

struct PaxosTest;

impl Workload for PaxosTest {
    fn name(&self) -> &str { "paxos-test" }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips();
        let client = PaxosClient::connect(servers, ctx.network()).await?;
        for i in 0..100 {
            client.propose(Value(i)).await?;
        }
        Ok(())
    }
    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // Verify linearizability
        Ok(())
    }
}

SimulationBuilder::new()
    .processes(5, || PaxosNode)
        .tags(&[("dc", &["east", "west", "eu"])])
    .workload(PaxosTest)
    .attrition(Attrition { max_dead: 1, prob_graceful: 0.3, prob_crash: 0.5, prob_wipe: 0.2 })
    .invariant(LinearizabilityCheck)
    .set_iterations(100)
    .run()
```

## Example: Orleans Actor Cluster

```rust
struct ActorHostProcess;

impl Process for ActorHostProcess {
    fn name(&self) -> &str { "actor-host" }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let host = ActorHost::new(ctx);
        host.register::<CounterActor>().await?;
        host.serve(ctx.shutdown()).await
    }
}

struct ActorTest;

impl Workload for ActorTest {
    fn name(&self) -> &str { "actor-test" }
    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let servers = ctx.topology().all_process_ips();
        let client = ActorClient::connect(servers, ctx.network()).await?;
        for i in 0..50 {
            let counter = client.get_actor::<CounterActor>(&format!("counter-{i}")).await?;
            counter.increment().await?;
        }
        Ok(())
    }
}

SimulationBuilder::new()
    .processes(3, || ActorHostProcess)
    .workload(ActorTest)
    .attrition(Attrition { max_dead: 1, prob_graceful: 0.5, prob_crash: 0.5, prob_wipe: 0.0 })
    .invariant(SingleActivation)
    .run()
```

## Non-Goals (Deferred)

- **Per-IP storage scoping**: simulate_crash() is currently global. Scoping
  storage per process IP is deferred.
- **Multiple Process types**: Single type per simulation. Role dispatch via tags.
- **Process-level storage wipe**: CrashAndWipe exists as an enum variant but
  storage wipe behavior is deferred until storage is scoped per IP.
- **Machine/host abstraction**: No multi-process-per-machine model. IP = process.
