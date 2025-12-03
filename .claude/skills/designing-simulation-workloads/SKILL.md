# Designing Simulation Workloads

## When to Use This Skill

Invoke this skill when you are:
- Creating new simulation tests for actor systems or distributed components
- Improving test coverage by expanding existing workloads
- Designing randomized operation sequences to explore state space
- Planning verification strategies (reference implementations, operation logs, invariants)
- Scaling workloads from single-node (1x1) to multi-node topologies (2x2, 10x10)

## Related Skills

- **using-buggify**: Add fault injection to force edge cases in your workload
- **using-chaos-assertions**: Track coverage and validate safety properties during execution
- **validating-with-invariants**: Design cross-workload properties for global validation

## Philosophy: From Test Cases to Autonomous Exploration

Traditional testing writes specific scenarios: "Do A, then B, verify C." This misses bugs hiding in unexpected combinations.

**Autonomous workload testing** shifts the approach: Define all possible operations (an "alphabet"), generate massive concurrent work, let deterministic chaos explore the state space.

### The Plinko Board Mental Model

Think of your system as a Plinko board:

```
     [Drop Zone - Inputs]
            │
    ○───○───○───○───○    ← Execution paths
      ○───○───○───○       ← State transitions
        ○───○───○         ← Decisions
          ○───○
    ┌───┬───┬───┬───┐
    │ 0 │$10│$50│ 0 │    ← Outcomes (success or bugs)
    └───┴───┴───┴───┘
```

- **Pegs** = Code paths, state transitions, decisions
- **Discs** = Work items (messages, requests, operations)
- **Buckets** = Outcomes (successful behavior OR bugs)

**Traditional testing**: Drop one disc down predefined paths → misses unexpected behavior

**Autonomous testing**: Dump an entire bucket of discs → find unexpected states through randomness and massive concurrency

## Four Principles of Autonomous Testing

### Principle 1: Build Properties, Not Test Cases

**Test Case Thinking** (limited):
```rust
assert!(insert("Alice", 100).is_ok());
assert!(get("Alice") == Some(100));
```

**Property Thinking** (general):
```rust
property!(valid_inserts_succeed,
    forall key: String, value: u64 =>
    is_valid(key, value) => insert(key, value).is_ok()
);
```

**For actor systems**, turn assumptions into testable properties:

```rust
// Assumption: "Actors activate only once"
always_assert!(
    no_duplicate_activation,
    activation_count(actor_id) <= 1,
    "Actor activated multiple times - race condition detected"
);

// Assumption: "Messages don't duplicate"
always_assert!(
    message_conservation,
    messages_received <= messages_sent,
    "Message duplication detected"
);
```

### Principle 2: Add Randomness (Data + Sequences)

**Level 1: Random Data**
```rust
let key = random.generate_string(1..100);
let value = random.range(0..u64::MAX);
```

**Level 2: Random Sequences** (more powerful!)
```rust
// Define alphabet, let simulation choose order
let operations = vec![
    Operation::Insert(random_key(), random_value()),
    Operation::Get(random_key()),
    Operation::Delete(random_key()),
    Operation::Update(random_key(), random_value()),
];

for _ in 0..1000 {
    let op = random.choice(&operations);
    execute(op);
}
```

**Why this matters**: Random sequences reveal race conditions. What if delete happens before insert? What if two activations race?

### Principle 3: Validate Often (Not Just at End)

**Traditional**: Check only at completion
```rust
run_test();
assert_eq!(final_state, expected); // ← Only here
```

**Autonomous**: Validate throughout execution
```rust
async fn workload() {
    sometimes_assert!(
        actors_making_progress,
        active_actors > 0,
        "At least one actor is active"
    );

    always_assert!(
        directory_consistent,
        directory.count(actor) <= 1,
        "Actor appears in multiple locations"
    );

    validate_final_properties();
}
```

**Why**: Assertions are signposts guiding exploration to find bugs faster.

### Principle 4: Generate Enough Work

**❌ Don't drop one disc at a time**:
```rust
for i in 0..10 {
    send_message(i);
    wait_for_response();
}
```

**✅ Dump the entire bucket**:
```rust
let num_operations = random.range(500..2000);
let mut tasks = vec![];

for _ in 0..num_operations {
    let op = random.choice(&operations);
    tasks.push(spawn_task(execute(op)));
}

join_all(tasks).await;
```

**Why**: Bugs hide in combinations and concurrency. Sequential execution misses them.

## The Operation Alphabet Pattern

The key to autonomous workloads: **Define all possible operations, let the fuzzer combine them.**

### Basic Template

```rust
enum Operation {
    // Actor lifecycle
    ActivateActor(ActorId),
    DeactivateActor(ActorId),

    // Messaging
    SendMessage(ActorId, Message),
    SendRequest(ActorId, Request),

    // State management
    SaveState(ActorId),
    LoadState(ActorId),

    // Infrastructure chaos (optional)
    CrashNode(NodeId),
    RestoreNode(NodeId),
}

async fn execute_operation(
    op: Operation,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        Operation::ActivateActor(id) => {
            runtime.activate_actor(id).await?;
            sometimes_assert!(actor_activated, true, "Actor activated");
        }
        Operation::SendMessage(id, msg) => {
            runtime.send_message(id, msg).await?;
            sometimes_assert!(message_sent, true, "Message sent");
        }
        // ... handle all operations
    }
    Ok(())
}
```

### Workload Structure

```rust
async fn autonomous_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(
        "test",
        network,
        time,
        task_provider.clone(),
    ).await?;

    // 1. Generate operation alphabet
    let mut operations = vec![];
    for _ in 0..100 {
        let actor_id = random_actor_id();
        operations.push(Operation::ActivateActor(actor_id));
        operations.push(Operation::SendMessage(actor_id, random_msg()));
        operations.push(Operation::DeactivateActor(actor_id));
    }

    // 2. Shuffle for randomness
    random.shuffle(&mut operations);

    // 3. Execute concurrently
    let mut tasks = vec![];
    for op in operations {
        let task = task_provider.spawn_task(
            execute_operation(op, &runtime)
        );
        tasks.push(task);
    }

    // 4. Wait for completion
    for task in tasks {
        task.await?;
    }

    // 5. Final validation
    validate_final_state(&runtime).await;

    Ok(SimulationMetrics::default())
}
```

## Three Verification Patterns

Choose the pattern that fits your system:

### Pattern 1: Reference Implementation

Mirror production logic with simple, correct implementation.

```rust
// Production: Complex distributed KV store
// Reference: std::HashMap

let mut reference = HashMap::new();
let distributed = DistributedKV::new();

// Apply same operations to both
for op in operations {
    match op {
        Insert(k, v) => {
            reference.insert(k, v);
            distributed.insert(k, v).await;
        }
        Get(k) => {
            let expected = reference.get(&k);
            let actual = distributed.get(&k).await;
            always_assert!(kv_match, actual == expected, "Mismatch");
        }
    }
}
```

### Pattern 2: Operation Logging

Record all operations, replay to verify consistency.

```rust
let mut log = Vec::new();

for op in operations {
    log.push(op.clone());
    system.execute(op).await;
}

// After execution, replay log and verify state
let final_state = system.get_state().await;
let expected = replay_operations(&log);
always_assert!(state_matches, final_state == expected, "Replay mismatch");
```

### Pattern 3: Invariant Tracking

Maintain mathematical properties that must hold.

```rust
// Example: Total balance conservation in banking system

let initial_balance: u64 = accounts.iter().map(|a| a.balance).sum();

// ... many operations (deposits, withdrawals, transfers) ...

let final_balance: u64 = accounts.iter().map(|a| a.balance).sum();

always_assert!(
    balance_conservation,
    final_balance == initial_balance + total_deposits - total_withdrawals,
    "Money conservation violated"
);
```

## Topology Scaling Strategy

Start simple, scale up progressively.

### 1x1 Topology (Basic Functionality)

```rust
SimulationBuilder::new()
    .register_workload("client", client_workload)
    .register_workload("server", server_workload)
    .run()
    .await;
```

**Tests**: Basic request-response, error handling, simple state transitions

### 2x2 Topology (Distributed Scenarios)

```rust
SimulationBuilder::new()
    .register_workload("client_1", client_workload)
    .register_workload("client_2", client_workload)
    .register_workload("server_1", server_workload)
    .register_workload("server_2", server_workload)
    .run()
    .await;
```

**Tests**: Multi-connection handling, load distribution, server switching, basic race conditions

### 10x10 Topology (Stress Testing)

```rust
async fn run_large_topology(
    num_clients: usize,
    num_servers: usize,
) -> SimulationReport {
    let mut builder = SimulationBuilder::new()
        .use_random_config()
        .set_iteration_control(
            IterationControl::UntilAllSometimesReached(10_000)
        );

    for i in 1..=num_servers {
        builder = builder.register_workload(
            format!("server_{}", i),
            server_workload
        );
    }

    for i in 1..=num_clients {
        builder = builder.register_workload(
            format!("client_{}", i),
            client_workload
        );
    }

    builder.run().await
}
```

**Tests**: Rare race conditions, queue overflow, network partition behavior, high contention

## ClientId-Based Work Partitioning

Use `topology.client_id` to partition work across multiple workload instances.

```rust
async fn partitioned_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
    topology: WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(/*...*/).await?;

    // Partition actor IDs by client_id
    let actor_ids: Vec<_> = (0..50)
        .filter(|i| i % topology.total_clients == topology.client_id)
        .map(|i| ActorId::virtual_actor("Test", &format!("actor_{}", i)))
        .collect();

    // Generate operations for this partition
    let mut operations = vec![];
    for actor_id in &actor_ids {
        operations.push(Operation::ActivateActor(actor_id.clone()));
        operations.push(Operation::SendMessage(actor_id.clone(), random_msg()));
    }

    // Execute...

    Ok(SimulationMetrics::default())
}
```

**Benefits**: Enables scaling tests (10+ clients) without operation conflicts.

## Simulation Test Setup

### Basic Test Structure

```rust
#[test]
fn slow_simulation_my_workload() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()  // Enable chaos
            .set_iteration_control(
                IterationControl::UntilAllSometimesReached(10_000)
            )
            .register_workload("my_workload", my_workload)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("Faulty seeds: {:?}", report.seeds_failing);
        }

        panic_on_assertion_violations(&report);
    });
}
```

### Iteration Control Strategies

```rust
// Run until all sometimes_assert! statements succeed at least once
IterationControl::UntilAllSometimesReached(10_000)

// Fixed number of seeds (quick smoke test)
IterationControl::FixedCount(10)

// Debug specific failing seed
SimulationBuilder::new()
    .set_seed(12345)
    .set_iteration_control(IterationControl::FixedCount(1))
```

### Debugging Failed Seeds

When a seed fails:

1. **Capture the seed**: Note from error output
2. **Single-seed replay** with detailed logging:
   ```rust
   let _ = tracing_subscriber::fmt()
       .with_max_level(Level::ERROR)
       .try_init();

   let report = SimulationBuilder::new()
       .set_seed(failing_seed)
       .set_iteration_control(IterationControl::FixedCount(1))
       .run()
       .await;
   ```
3. **Examine error**: Read stack trace and error message
4. **Fix root cause**: Don't just work around the symptom
5. **Re-enable chaos**: Verify fix under full randomness

## Integration Checklist

When creating a new simulation workload:

- [ ] Define operation alphabet (enum) with all possible operations
- [ ] Implement `execute_operation()` matching on each variant
- [ ] Add `sometimes_assert!` for coverage tracking (See: using-chaos-assertions skill)
- [ ] Add `always_assert!` for safety invariants
- [ ] Generate 500-2000 concurrent operations
- [ ] Shuffle operations for randomness
- [ ] Execute operations concurrently via `spawn_task`
- [ ] Add strategic `buggify!` calls (See: using-buggify skill)
- [ ] Validate final state properties
- [ ] Configure test with `slow_simulation` prefix in name
- [ ] Set timeout in `.config/nextest.toml` (240s recommended)
- [ ] Start with 1x1 topology, scale to 2x2, then 10x10
- [ ] Use `UntilAllSometimesReached(10_000)` for comprehensive coverage

## Practical Guidelines

### Start Small, Scale Up

```rust
// Phase 1: Basic alphabet (5-10 operations)
enum BasicOps {
    Activate(ActorId),
    Deactivate(ActorId),
    SendMessage(ActorId, Msg),
}

// Phase 2: Add complexity incrementally
// Phase 3: Increase concurrency gradually
//   Start: 100 operations
//   Then: 500 operations
//   Finally: 1000+ operations
```

### Balance Exploration vs Test Time

```rust
// Quick smoke test (development)
let num_ops = 100;
let iterations = IterationControl::FixedCount(10);

// Thorough testing (CI)
let num_ops = 500;
let iterations = IterationControl::UntilAllSometimesReached(1000);

// Comprehensive (nightly)
let num_ops = 1000;
let iterations = IterationControl::UntilAllSometimesReached(10_000);
```

### Debug Strategy

When tests fail:

1. **Capture seed**: From error output
2. **Reduce operations**: `let num_ops = 10;` to simplify
3. **Add logging**: Trace operation execution
4. **Replay deterministically**: `set_seed(failing_seed)`
5. **Binary search**: Reduce until bug disappears, then analyze

## Key Takeaways

- **Think in operations, not scenarios**: Define alphabet, let simulation explore
- **Properties over test cases**: Make assumptions explicit as assertions
- **Massive concurrency**: Dump the bucket, don't drop one disc
- **Validate throughout**: Assertions guide exploration
- **Randomness is key**: Both data AND sequences
- **Start simple, scale up**: Gradual complexity increase

The goal is autonomous state space exploration, finding bugs you couldn't imagine!

## Additional Resources

See separate reference files:
- `EXAMPLES.md`: Three complete workload examples (Bank Account, Directory, MessageBus)
- `PATTERNS.md`: Detailed operation alphabet implementations
- `VERIFICATION.md`: Deep dive into the three verification patterns
