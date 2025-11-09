---
name: moonpool-chaos-testing
description: Add buggify fault injection and assertion coverage tracking to moonpool actor system code using moonpool-foundation testing patterns
---

# Moonpool Chaos Testing Skill

This skill teaches you how to add **deterministic chaos testing** to the moonpool actor system using the moonpool-foundation testing infrastructure.

## Philosophy: Find Bugs Before Production

The moonpool-foundation testing framework is inspired by FoundationDB's simulation testing approach:

- **Deterministic**: Same seed = identical behavior (reproducible bugs)
- **Chaos by default**: 10x worse than production conditions
- **Comprehensive coverage**: Test all error paths via assertions
- **100% success rate**: No deadlocks or hangs acceptable

**Goal**: Find bugs during development through hostile infrastructure simulation.

## Core Testing Primitives

### 1. Buggify - Deterministic Fault Injection

**Location**: `moonpool-foundation/src/buggify.rs`

#### How It Works

Buggify provides location-based fault injection:

1. Each `buggify!()` location is randomly **activated** once per simulation run
2. Active locations **fire** probabilistically on each call
3. Same seed = same activation decisions (deterministic)

```rust
// Example from foundation/src/network/peer/core.rs:892
if buggify!() {
    tracing::warn!("Buggify: Triggering connection failure");
    return Err(ConnectionError::Buggified);
}
```

#### Macro API

```rust
// Default 25% probability when active
buggify!()

// Custom probability
buggify_with_prob!(0.5)  // 50% when active
buggify_with_prob!(0.75) // 75% when active
```

#### Strategic Placement Guide

**Where to add buggify**:

1. **Error Handling Paths**
   ```rust
   // Trigger connection failures
   if buggify!() {
       return Err(NetworkError::ConnectionFailed);
   }
   ```

2. **Timeout Triggers**
   ```rust
   // Force timeout scenarios
   let timeout = if buggify!() {
       Duration::from_millis(1) // Very short
   } else {
       Duration::from_secs(5)   // Normal
   };
   ```

3. **State Transitions**
   ```rust
   // Delay state changes
   if buggify!() {
       time.sleep(Duration::from_millis(100)).await;
   }
   ```

4. **Resource Limits**
   ```rust
   // Trigger queue capacity scenarios
   let max_queue = if buggify!() {
       2  // Very small
   } else {
       1000
   };
   ```

5. **Network Operations**
   ```rust
   // Simulate send failures
   if buggify!() {
       return Err(TransportError::SendFailed);
   }
   ```

#### Actor System Examples

**MessageBus routing**:
```rust
// In routing logic
if buggify!() {
    tracing::warn!("Buggify: Simulating directory lookup failure");
    // Force actor placement to different node
}
```

**ActorCatalog activation**:
```rust
// During actor creation
if buggify!() {
    time.sleep(Duration::from_millis(50)).await; // Race condition window
}
```

**Directory registration**:
```rust
// Before registering location
if buggify!() {
    return Err(DirectoryError::RegistrationFailed);
}
```

#### From Ping Pong Example

The ping_pong test uses buggify in `tests.rs:311-314`:
```rust
// 50% chance of very short timeout to cause backlog
let timeout_ms = if self.random.random_bool(0.5) {
    self.random.random_range(1..10) // Very short: 1-10ms
} else {
    self.random.random_range(100..5000) // Normal: 100-5000ms
};
```

Note: This uses `random.random_bool()` instead of `buggify!()` but serves the same purpose - creating chaos.

### 2. Assertions - Coverage Tracking

**Location**: `moonpool-foundation/src/assertions.rs`

#### Two Types of Assertions

**`sometimes_assert!`** - Track coverage across seeds:
- Records success rate statistics
- Must succeed at least once across all test runs
- Used for probabilistic properties

**`always_assert!`** - Safety invariants:
- Panics immediately on failure
- Used for properties that must never be violated
- Provides seed for reproduction

#### Macro API

```rust
// Sometimes assertion - track coverage
sometimes_assert!(
    name_identifier,
    boolean_condition,
    "Description of what this tests"
);

// Always assertion - crash on failure
always_assert!(
    name_identifier,
    boolean_condition,
    "Error message on failure"
);
```

#### When to Use Each

**Use `sometimes_assert!` for**:
- Error paths that should trigger under chaos
- Multi-actor scenarios (multiple connections, server switching)
- Resource pressure conditions
- Timing-dependent behavior

**Use `always_assert!` for**:
- State consistency checks
- Message ordering guarantees
- Conservation laws (messages sent <= messages received)
- Uniqueness constraints

#### Concrete Examples from Ping Pong

**From `ping_pong/actors.rs`**:

1. **Multi-connection handling** (line 118-122):
```rust
sometimes_assert!(
    server_handles_multiple_connections,
    transport.connection_count() > 1,
    "Server handling multiple connections"
);
```

2. **Connection active** (line 125-128):
```rust
sometimes_assert!(
    server_connection_active,
    true,
    "Server connection is active"
);
```

3. **High message rate** (line 138-142):
```rust
sometimes_assert!(
    server_high_message_rate,
    self.pings_received > 5,
    "Server handling high message rate"
);
```

4. **Client switching servers** (line 292-297):
```rust
sometimes_assert!(
    client_switches_servers,
    selected_server != last_server,
    "Client switching between servers"
);
```

5. **Request timeout** (line 369-373):
```rust
sometimes_assert!(
    client_timeout_occurred,
    true,
    "Client request timeout occurred"
);
```

#### Actor System Assertion Ideas

**ActorCatalog**:
```rust
sometimes_assert!(
    catalog_concurrent_activation,
    self.actors.len() > 5,
    "Catalog handling concurrent activations"
);
```

**MessageBus**:
```rust
sometimes_assert!(
    bus_remote_routing,
    node != self.node_id,
    "MessageBus routing to remote node"
);
```

**Directory**:
```rust
always_assert!(
    directory_unique_location,
    !self.registrations.contains_key(&actor_id) ||
    self.registrations[&actor_id] == node_id,
    "Actor registered to multiple nodes"
);
```

#### Success Rate Validation

After simulation, the framework validates that `sometimes_assert!` statements succeeded at least once:

```rust
// From assertions.rs:156-171
pub fn validate_assertion_contracts() -> Vec<String> {
    let mut violations = Vec::new();
    let results = get_assertion_results();

    for (name, stats) in &results {
        let rate = stats.success_rate();
        if stats.total_checks > 0 && rate == 0.0 {
            violations.push(format!(
                "sometimes_assert!('{}') has {:.1}% success rate (expected at least 1%)",
                name, rate
            ));
        }
    }
    violations
}
```

If a `sometimes_assert!` never succeeds, it means that code path was never tested!

## 3. Simulation Testing Setup

**Location**: `moonpool-foundation/tests/simulation/ping_pong/tests.rs`

#### Basic Structure

```rust
use moonpool_foundation::{
    SimulationBuilder, SimulationResult, SimulationMetrics,
    runner::IterationControl,
    assertions::panic_on_assertion_violations,
};

#[test]
fn slow_simulation_actor_test() {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move {
        let report = SimulationBuilder::new()
            .use_random_config()  // Enable chaos
            .set_iteration_control(IterationControl::UntilAllSometimesReached(10_000))
            .register_workload("actor_workload", actor_workload)
            .run()
            .await;

        println!("{}", report);

        if !report.seeds_failing.is_empty() {
            panic!("faulty seeds detected: {:?}", report.seeds_failing);
        }

        // Validate assertions
        panic_on_assertion_violations(&report);
    });
}
```

#### Workload Signature

```rust
async fn actor_workload(
    random: moonpool_foundation::random::sim::SimRandomProvider,
    provider: moonpool_foundation::SimNetworkProvider,
    time_provider: moonpool_foundation::SimTimeProvider,
    task_provider: moonpool_foundation::TokioTaskProvider,
    topology: moonpool_foundation::WorkloadTopology,
) -> SimulationResult<SimulationMetrics> {
    // Create actor runtime with sim providers
    let runtime = ActorRuntime::with_providers(
        "test",
        provider,
        time_provider,
        task_provider,
    ).await?;

    // Run test logic...

    Ok(SimulationMetrics::default())
}
```

#### Multi-Topology Testing

Test with different actor counts to trigger different scenarios:

```rust
// From ping_pong/tests.rs:205-227
async fn run_ping_pong_simulation(
    num_clients: usize,
    num_servers: usize,
    max_iterations: usize,
) -> moonpool_foundation::SimulationReport {
    let mut builder = SimulationBuilder::new()
        .use_random_config()
        .set_iteration_control(IterationControl::UntilAllSometimesReached(max_iterations));

    // Register servers
    for i in 1..=num_servers {
        builder = builder.register_workload(format!("server_{}", i), server_workload);
    }

    // Register clients
    for i in 1..=num_clients {
        builder = builder.register_workload(format!("client_{}", i), client_workload);
    }

    builder.run().await
}
```

**Topology patterns**:
- **1x1**: Basic functionality
- **2x2**: Distributed scenarios, multi-connection
- **10x10**: Stress testing, race conditions

#### Iteration Control Strategies

```rust
// Run until all sometimes_assert! statements succeed
IterationControl::UntilAllSometimesReached(10_000)

// Fixed number of seeds for quick testing
IterationControl::FixedCount(10)

// Debug specific failing seed
SimulationBuilder::new()
    .set_seed(12345)
    .set_iteration_control(IterationControl::FixedCount(1))
```

#### Debugging Failed Seeds

When a test fails:

1. Note the failing seed from the output
2. Set up single-seed run with ERROR logging:
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
3. Examine the error message and stack trace
4. Fix the root cause
5. Re-enable chaos testing

## 4. Autonomous Workload Design Philosophy

This section teaches you how to design simulation workloads that **autonomously explore** your actor system's state space, inspired by Antithesis's approach to deterministic testing.

### The Mental Model: Your System as a Plinko Board

Think of your actor system as a Plinko board:

```
     [Drop Zone - Inputs]
            │
    ○───○───○───○───○    ← Execution paths (code)
      ○───○───○───○
        ○───○───○       ← State transitions
          ○───○
    ┌───┬───┬───┬───┐
    │ 0 │$10│$50│ 0 │    ← Outcomes (bugs or success)
    └───┴───┴───┴───┘
```

- **Pegs** = Code execution paths, state transitions, decisions
- **Discs** = Work items (messages, requests, operations)
- **Buckets** = Outcomes (successful outcomes OR bugs)

**Traditional testing** drops one disc at a time down predefined paths:
- You test specific scenarios
- Miss the "triangular pegs" (unexpected behavior)
- Miss the "black holes" (crash states)
- Large portions of board never tested

**Autonomous testing** dumps an entire bucket of discs:
- System explores all paths through randomness
- Finds unexpected states (triangular pegs)
- Discovers crash conditions (black holes)
- Comprehensive state space coverage

### From Automated to Autonomous

**Automated testing**: Computer remembers to run your tests
```rust
// Automated: predefined path
#[test]
fn test_specific_scenario() {
    let actor = create_actor();
    send_message(&actor, msg1);  // ← Predefined
    send_message(&actor, msg2);  // ← Predefined
    assert_eq!(actor.state(), expected); // ← Predefined
}
```

**Autonomous testing**: Computer explores state space to find interesting states
```rust
// Autonomous: computer explores
#[test]
fn test_actor_properties() {
    let operations = all_possible_operations();

    for _ in 0..1000 {
        let op = random.choice(&operations); // ← Computer chooses
        execute(op);
        validate_properties(); // ← Check along the way
    }
}
```

### Four Principles of Autonomous Testing

#### Principle 1: Build Properties, Not Test Cases

**Test Case Thinking** (limited):
```rust
// Test case: specific inputs
assert!(insert("Alice", 100).is_ok());
assert!(insert("Bob", 200).is_ok());
assert!(get("Alice") == Some(100));
```

**Property Thinking** (general):
```rust
// Property: universal statements
property!(valid_inserts_succeed,
    forall key: String, value: u64 =>
    is_valid(key, value) => insert(key, value).is_ok()
);

property!(get_returns_last_insert,
    forall key: String, value: u64 =>
    insert(key, value).is_ok() => get(key) == Some(value)
);
```

**For Actor Systems**, turn assumptions into properties:

```rust
// Assumption: "Actors activate only once"
// Property:
always_assert!(
    no_duplicate_activation,
    !catalog.contains(&actor_id) || catalog[&actor_id].state == Activating,
    "Actor activated twice - race condition"
);

// Assumption: "Messages don't duplicate"
// Property:
always_assert!(
    message_conservation,
    messages_received <= messages_sent,
    "Message duplication detected"
);

// Assumption: "Directory is eventually consistent"
// Property:
property!(directory_consistency,
    forall actor_id: ActorId =>
    directory.lookup(actor_id).count() <= 1
);
```

#### Principle 2: Add Randomness (Data + Sequences)

**Level 1: Random Data**
```rust
// Instead of specific values
let key = "Alice";
let value = 100;

// Use generators
let key = random.generate_string(1..100);
let value = random.range(0..u64::MAX);
```

**Level 2: Random Sequences** (more powerful!)
```rust
// ❌ Don't prescribe order
insert("A", 1);
get("A");
delete("A");

// ✅ Define alphabet, let fuzzer choose order
let operations = vec![
    Operation::Insert(random_key(), random_value()),
    Operation::Get(random_key()),
    Operation::Delete(random_key()),
    Operation::Update(random_key(), random_value()),
];

// Computer chooses sequence
for _ in 0..1000 {
    let op = random.choice(&operations);
    execute(op);
}
```

**For Actors**: Random operation sequences reveal race conditions
```rust
// Define ALL actor operations
enum ActorOp {
    Activate(ActorId),
    Deactivate(ActorId),
    SendMessage(ActorId, Message),
    RequestResponse(ActorId, Request),
    SaveState(ActorId),
    CrashNode(NodeId),
}

// Generate random interleaving
for _ in 0..1000 {
    spawn_task(execute(random.choice(&ops)));
}
```

This finds bugs like:
- What if delete happens before insert?
- What if two activations race?
- What if deactivation happens during message send?

#### Principle 3: Validate Often (Not Just at End)

**Traditional**: Validate only at test completion
```rust
run_test();
assert_eq!(final_state, expected); // ← Only check here
```

**Autonomous**: Validate throughout execution
```rust
async fn workload() {
    // Check progress
    sometimes_assert!(
        actors_making_progress,
        active_actors > 0,
        "At least one actor is active"
    );

    // Check invariants during execution
    always_assert!(
        directory_consistent,
        directory.count(actor) <= 1,
        "Actor in multiple locations"
    );

    // Mark unreachable states
    if state == Impossible {
        unreachable!("Should never reach this state");
    }

    // Still validate at end
    validate_final_properties();
}
```

**Why this matters**: Assertions are **signposts** for the computer
- `sometimes_assert!` → "This state is interesting, try to reach it"
- `always_assert!` → "This invariant must hold, violation is a bug"
- `unreachable!` → "If you get here, something is very wrong"

These guide autonomous exploration to find bugs faster.

#### Principle 4: Generate Enough Work

**❌ Don't drop one disc at a time**:
```rust
// Sequential, limited exploration
for i in 0..10 {
    send_message(i);
    wait_for_response();
}
```

**✅ Dump the entire bucket**:
```rust
// Massive concurrency, deep exploration
let num_operations = random.range(500..2000);
let mut tasks = vec![];

for _ in 0..num_operations {
    let op = random.choice(&operations);
    tasks.push(spawn_task(execute(op)));
}

// Let chaos ensue
join_all(tasks).await;
```

**Why**: Bugs hide in combinations and concurrency
- Race conditions need concurrent operations
- Deadlocks need specific interleavings
- Resource exhaustion needs high load
- One-at-a-time testing misses these

### Making Assumptions Explicit

Every actor system has **implicit assumptions**. Autonomous testing makes them **explicit as properties**.

**Example: Actor Activation Assumptions**

```rust
// Implicit assumption: "Each actor activates exactly once"
// Make explicit:
always_assert!(
    single_activation,
    activation_count(actor_id) <= 1,
    "Actor activated multiple times"
);

// Implicit assumption: "Activation completes before message delivery"
// Make explicit:
always_assert!(
    activation_before_message,
    !catalog.contains(actor_id) || catalog[actor_id].state == Active,
    "Message delivered to non-active actor"
);

// Implicit assumption: "Deactivation releases resources"
// Make explicit:
always_assert!(
    resources_released,
    deactivated_actors.iter().all(|id| !resources.contains(id)),
    "Deactivated actor still holds resources"
);
```

**Example: Message Routing Assumptions**

```rust
// Implicit: "Messages reach destination eventually"
sometimes_assert!(
    messages_delivered,
    messages_received > 0,
    "At least one message delivered"
);

// Implicit: "No message duplication"
always_assert!(
    no_duplication,
    messages_received <= messages_sent,
    "Message duplication detected"
);

// Implicit: "Round-trip requests get responses"
sometimes_assert!(
    requests_completed,
    responses_received > 0,
    "Request-response completed"
);
```

### Operation Alphabet Pattern

The key to autonomous workloads: Define **all possible operations**, let the fuzzer combine them.

#### Template

```rust
// 1. Define your operation alphabet
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

    // Infrastructure
    CrashNode(NodeId),
    RestoreNode(NodeId),
    PartitionNetwork(NodeId, NodeId),
    HealPartition(NodeId, NodeId),
}

// 2. Operation execution
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

// 3. Workload: generate random combinations
async fn autonomous_workload(
    random: SimRandomProvider,
    // ... other providers
) -> SimulationResult<SimulationMetrics> {
    // Create all possible operations
    let mut operations = vec![];

    // Generate diverse operation mix
    for _ in 0..100 {
        let actor_id = random_actor_id();
        operations.push(Operation::ActivateActor(actor_id));
        operations.push(Operation::SendMessage(actor_id, random_msg()));
        operations.push(Operation::DeactivateActor(actor_id));
    }

    // Shuffle and execute with massive concurrency
    random.shuffle(&mut operations);

    let mut tasks = vec![];
    for op in operations {
        // Spawn concurrent task
        let task = task_provider.spawn_task(
            execute_operation(op, &runtime)
        );
        tasks.push(task);

        // Add chaos
        if buggify!() {
            time.sleep(Duration::from_millis(random.range(1..100))).await;
        }
    }

    // Wait for all
    for task in tasks {
        task.await?;
    }

    Ok(SimulationMetrics::default())
}
```

### Concrete Examples

#### Example 1: Bank Account Autonomous Workload

```rust
// Operation alphabet for bank accounts
enum BankOp {
    CreateAccount(AccountId, u64),    // id, initial_balance
    Deposit(AccountId, u64),           // id, amount
    Withdraw(AccountId, u64),          // id, amount
    Transfer(AccountId, AccountId, u64), // from, to, amount
    CheckBalance(AccountId),
    CloseAccount(AccountId),
}

async fn bank_account_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(
        "bank",
        network,
        time,
        task_provider.clone(),
    ).await?;

    // Generate 1000 random banking operations
    let num_ops = 1000;
    let account_ids: Vec<_> = (0..20)
        .map(|i| format!("account_{}", i))
        .collect();

    let mut operations = vec![];
    for _ in 0..num_ops {
        let op = match random.range(0..6) {
            0 => BankOp::CreateAccount(
                random.choice(&account_ids).clone(),
                random.range(0..10000),
            ),
            1 => BankOp::Deposit(
                random.choice(&account_ids).clone(),
                random.range(1..1000),
            ),
            2 => BankOp::Withdraw(
                random.choice(&account_ids).clone(),
                random.range(1..1000),
            ),
            3 => BankOp::Transfer(
                random.choice(&account_ids).clone(),
                random.choice(&account_ids).clone(),
                random.range(1..1000),
            ),
            4 => BankOp::CheckBalance(
                random.choice(&account_ids).clone(),
            ),
            5 => BankOp::CloseAccount(
                random.choice(&account_ids).clone(),
            ),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    // Execute with massive concurrency
    let mut tasks = vec![];
    for op in operations {
        let runtime_clone = runtime.clone();
        let task = task_provider.spawn_task(async move {
            execute_bank_op(op, &runtime_clone).await
        });
        tasks.push(task);
    }

    // Wait for completion
    for task in tasks {
        let _ = task.await; // Ignore errors, focus on properties
    }

    // Validate properties
    validate_bank_invariants(&runtime).await;

    Ok(SimulationMetrics::default())
}

async fn execute_bank_op(
    op: BankOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        BankOp::CreateAccount(id, balance) => {
            runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(CreateAccountMsg { initial_balance: balance })
                .await?;

            sometimes_assert!(
                account_created,
                true,
                "Bank account created"
            );
        }
        BankOp::Deposit(id, amount) => {
            runtime.get_actor::<BankAccount>("BankAccount", &id)
                .call(DepositMsg { amount })
                .await?;

            sometimes_assert!(
                deposit_succeeded,
                amount > 0,
                "Deposit with positive amount"
            );
        }
        // ... other operations
    }
    Ok(())
}

async fn validate_bank_invariants(runtime: &ActorRuntime) {
    // Property: Total money in system is conserved
    let total_deposits = get_total_deposits(runtime).await;
    let total_withdrawals = get_total_withdrawals(runtime).await;
    let current_balances = get_all_balances(runtime).await;

    always_assert!(
        money_conservation,
        current_balances == total_deposits - total_withdrawals,
        "Money conservation violated"
    );
}
```

#### Example 2: Directory Consistency Workload

```rust
// Operations that affect directory state
enum DirectoryOp {
    ActivateActor(ActorId, NodeId),    // Registers in directory
    DeactivateActor(ActorId),           // Unregisters
    MigrateActor(ActorId, NodeId),      // Changes location
    LookupActor(ActorId),               // Query
    CrashNode(NodeId),                  // Remove all actors on node
}

async fn directory_chaos_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
) -> SimulationResult<SimulationMetrics> {
    let runtime = ActorRuntime::with_providers(
        "directory_test",
        network,
        time,
        task_provider.clone(),
    ).await?;

    let actor_ids: Vec<_> = (0..50)
        .map(|i| ActorId::virtual_actor("TestActor", &format!("actor_{}", i)))
        .collect();

    let node_ids = vec![NodeId(1), NodeId(2), NodeId(3)];

    // Generate 2000 directory operations
    let mut operations = vec![];
    for _ in 0..2000 {
        let op = match random.range(0..5) {
            0 => DirectoryOp::ActivateActor(
                random.choice(&actor_ids).clone(),
                random.choice(&node_ids).clone(),
            ),
            1 => DirectoryOp::DeactivateActor(
                random.choice(&actor_ids).clone(),
            ),
            2 => DirectoryOp::MigrateActor(
                random.choice(&actor_ids).clone(),
                random.choice(&node_ids).clone(),
            ),
            3 => DirectoryOp::LookupActor(
                random.choice(&actor_ids).clone(),
            ),
            4 => DirectoryOp::CrashNode(
                random.choice(&node_ids).clone(),
            ),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    // Massive concurrency to trigger races
    let mut tasks = vec![];
    for op in operations {
        let runtime_clone = runtime.clone();
        let task = task_provider.spawn_task(async move {
            // Validate invariant DURING execution
            validate_directory_invariant(&runtime_clone).await;

            execute_directory_op(op, &runtime_clone).await;

            // Validate again AFTER operation
            validate_directory_invariant(&runtime_clone).await;
        });
        tasks.push(task);

        // Random delays to vary interleaving
        if buggify!() {
            time.sleep(Duration::from_millis(random.range(1..50))).await;
        }
    }

    // Wait for chaos to complete
    for task in tasks {
        let _ = task.await;
    }

    Ok(SimulationMetrics::default())
}

async fn validate_directory_invariant(runtime: &ActorRuntime) {
    // Property: Each actor in at most one location
    let directory = runtime.directory();
    let all_actors = directory.get_all_actors().await;

    for actor_id in all_actors {
        let locations = directory.lookup(&actor_id).await.unwrap();

        always_assert!(
            single_location,
            locations.len() <= 1,
            format!("Actor {:?} in {} locations", actor_id, locations.len())
        );
    }
}

async fn execute_directory_op(
    op: DirectoryOp,
    runtime: &ActorRuntime,
) -> Result<()> {
    match op {
        DirectoryOp::ActivateActor(actor_id, node_id) => {
            runtime.directory().register(&actor_id, node_id).await?;

            sometimes_assert!(
                actor_registered,
                true,
                "Actor registered in directory"
            );
        }
        DirectoryOp::MigrateActor(actor_id, new_node) => {
            runtime.directory().unregister(&actor_id).await?;
            runtime.directory().register(&actor_id, new_node).await?;

            sometimes_assert!(
                actor_migrated,
                true,
                "Actor migrated to new node"
            );
        }
        // ... other operations
    }
    Ok(())
}
```

#### Example 3: MessageBus Routing Chaos

```rust
enum RoutingOp {
    SendMessage(ActorId, Message),
    SendRequest(ActorId, Request),
    ActivateRemoteActor(ActorId, NodeId),
    PartitionNetwork(NodeId, NodeId),
    HealPartition(NodeId, NodeId),
    CrashMessageBus(NodeId),
    RestoreMessageBus(NodeId),
}

async fn routing_chaos_workload(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
) -> SimulationResult<SimulationMetrics> {
    // Setup multi-node cluster
    let nodes = vec![
        create_runtime("node_1", "127.0.0.1:5001", network.clone(), time.clone(), task_provider.clone()).await?,
        create_runtime("node_2", "127.0.0.1:5002", network.clone(), time.clone(), task_provider.clone()).await?,
        create_runtime("node_3", "127.0.0.1:5003", network.clone(), time.clone(), task_provider.clone()).await?,
    ];

    let actor_ids: Vec<_> = (0..30)
        .map(|i| ActorId::virtual_actor("Router", &format!("router_{}", i)))
        .collect();

    // Generate 1500 routing operations with network chaos
    let mut operations = vec![];
    for _ in 0..1500 {
        let op = match random.range(0..7) {
            0 | 1 | 2 => RoutingOp::SendMessage(
                random.choice(&actor_ids).clone(),
                random_message(),
            ),
            3 => RoutingOp::SendRequest(
                random.choice(&actor_ids).clone(),
                random_request(),
            ),
            4 => RoutingOp::PartitionNetwork(
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
            ),
            5 => RoutingOp::HealPartition(
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
            ),
            6 => RoutingOp::ActivateRemoteActor(
                random.choice(&actor_ids).clone(),
                random.choice(&[NodeId(1), NodeId(2), NodeId(3)]),
            ),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    // Execute with chaos
    let mut tasks = vec![];
    for op in operations {
        let nodes_clone = nodes.clone();
        let task = task_provider.spawn_task(async move {
            execute_routing_op(op, &nodes_clone).await;

            // Validate routing properties
            sometimes_assert!(
                messages_routed,
                true,
                "Message routing attempted"
            );
        });
        tasks.push(task);
    }

    for task in tasks {
        let _ = task.await;
    }

    // Final validation
    validate_message_conservation(&nodes).await;

    Ok(SimulationMetrics::default())
}

async fn validate_message_conservation(nodes: &[ActorRuntime]) {
    let total_sent: u64 = nodes.iter()
        .map(|n| n.message_bus().metrics().messages_sent)
        .sum();
    let total_received: u64 = nodes.iter()
        .map(|n| n.message_bus().metrics().messages_received)
        .sum();

    always_assert!(
        message_conservation,
        total_received <= total_sent,
        "Message conservation violated: {} received > {} sent",
        total_received,
        total_sent
    );
}
```

### Comparison: Constrained vs Autonomous

#### Ping Pong: Current (Constrained)

```rust
// From ping_pong/tests.rs - current approach
async fn ping_pong_client(/*...*/) -> SimulationResult<SimulationMetrics> {
    let mut client = PingPongClientActor::new(/*...*/);

    // Constrained: exactly 10 pings
    for _ in 0..MAX_PING_PER_CLIENT {  // ← Fixed count
        client.run_single_ping().await?;  // ← Sequential
    }

    Ok(SimulationMetrics::default())
}
```

**Limitations**:
- Fixed operation count (10 pings)
- Sequential execution (one ping at a time)
- Predictable pattern (ping, wait, ping, wait...)
- Limited state space exploration

#### Ping Pong: Enhanced (Autonomous)

```rust
// Enhanced autonomous version
enum PingPongOp {
    SendPing(ServerId),
    ConnectToServer(ServerId),
    DisconnectFromServer(ServerId),
    SwitchPrimaryServer(ServerId),
    CrashServer(ServerId),
    RestoreServer(ServerId),
}

async fn autonomous_ping_pong_client(
    random: SimRandomProvider,
    network: SimNetworkProvider,
    time: SimTimeProvider,
    task_provider: TokioTaskProvider,
) -> SimulationResult<SimulationMetrics> {
    let client = PingPongClientActor::new(/*...*/);

    // Generate 500-1000 random operations
    let num_ops = random.range(500..1000);
    let mut operations = vec![];

    for _ in 0..num_ops {
        let op = match random.range(0..6) {
            0 | 1 | 2 => PingPongOp::SendPing(random_server()),
            3 => PingPongOp::SwitchPrimaryServer(random_server()),
            4 => PingPongOp::DisconnectFromServer(random_server()),
            5 => PingPongOp::ConnectToServer(random_server()),
            _ => unreachable!(),
        };
        operations.push(op);
    }

    // Execute with massive concurrency
    let mut tasks = vec![];
    for op in operations {
        let client_clone = client.clone();
        let task = task_provider.spawn_task(async move {
            execute_ping_pong_op(op, &client_clone).await;

            // Validate throughout
            sometimes_assert!(rapid_pings, true, "Rapid ping execution");
        });
        tasks.push(task);

        // Add timing chaos
        if buggify!() {
            time.sleep(Duration::from_millis(random.range(1..100))).await;
        }
    }

    for task in tasks {
        let _ = task.await;
    }

    Ok(SimulationMetrics::default())
}
```

**Benefits**:
- Variable operation count (500-1000)
- Massive concurrency (all operations spawned)
- Unpredictable patterns (random order, random timing)
- Deep state space exploration

**What This Finds**:
- Connection race conditions (connect + disconnect simultaneously)
- Server switching bugs (change server mid-ping)
- Queue overflow (rapid concurrent pings)
- Timeout handling (chaotic timing)
- Network partition behavior

### Practical Guidelines

#### Start Small, Scale Up

```rust
// 1. Start with basic alphabet (5-10 operations)
enum BasicOps {
    Activate(ActorId),
    Deactivate(ActorId),
    SendMessage(ActorId, Msg),
}

// 2. Add operations incrementally
enum ExpandedOps {
    Activate(ActorId),
    Deactivate(ActorId),
    SendMessage(ActorId, Msg),
    // Add complexity gradually
    SaveState(ActorId),
    CrashNode(NodeId),
}

// 3. Increase concurrency gradually
// Start: 100 operations
// Then: 500 operations
// Finally: 1000+ operations
```

#### Balance Exploration vs Test Time

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

#### Debug Strategies

When autonomous tests fail:

1. **Capture the seed**: Failing seed is in the error output
2. **Reduce operation count**: `let num_ops = 10;` to simplify
3. **Add logging**: Trace operation execution
4. **Replay deterministically**: `set_seed(failing_seed)`
5. **Binary search**: Reduce operation count until bug disappears

```rust
// Debug configuration
SimulationBuilder::new()
    .set_seed(123456)  // ← Reproducible
    .set_iteration_control(IterationControl::FixedCount(1))  // ← Single run
    .register_workload("debug", debug_workload)
    .run()
    .await;
```

### Key Takeaways

1. **Think in operations, not scenarios**: Define alphabet, let fuzzer explore
2. **Properties over test cases**: Make assumptions explicit
3. **Massive concurrency**: Dump the bucket, don't drop one disc
4. **Validate throughout**: Assertions are signposts for exploration
5. **Randomness is key**: Both data and sequences
6. **Start simple, scale up**: Gradual complexity increase

**Remember**: The goal is to let the computer explore your state space autonomously, finding bugs you couldn't think of!

## Integration Checklist

### 1. Add Foundation Dependency

**In `moonpool/Cargo.toml`**:
```toml
[dependencies]
moonpool-foundation = { path = "../moonpool-foundation" }
```

### 2. Import Macros

**At the top of your test file**:
```rust
use moonpool_foundation::{
    buggify, buggify_with_prob,
    sometimes_assert, always_assert,
};
```

### 3. Add Buggify Calls

Place strategically in error paths, state transitions, etc.

### 4. Add Assertions

- `sometimes_assert!` for coverage
- `always_assert!` for safety

### 5. Create Simulation Test

- Use `SimulationBuilder`
- Register workloads
- Set iteration control
- Validate assertions

### 6. Test Configuration

**In `.config/nextest.toml`**:
```toml
[[profile.default.overrides]]
filter = 'test(slow_simulation)'
slow-timeout = { period = "240s" }
```

### 7. Run Tests

```bash
# All tests
nix develop --command cargo nextest run -p moonpool

# Specific simulation test
nix develop --command cargo nextest run -p moonpool test_name
```

## Best Practices

### Buggify

1. **Don't overuse**: Too many buggify calls slow down simulations
2. **Strategic placement**: Focus on error paths and race conditions
3. **Probability tuning**: Use higher probability (0.5-0.75) for rare events
4. **Document intent**: Add comment explaining what chaos you're creating

### Assertions

1. **Unique names**: Each assertion needs a distinct identifier
2. **Clear messages**: Explain what property is being tested
3. **Atomic conditions**: Test one thing per assertion
4. **Balance coverage**: Don't assert on every line, focus on key scenarios

### Simulation Testing

1. **Start simple**: 1x1 topology first, then expand
2. **Iterate count**: Use 10,000 for comprehensive coverage
3. **Timeout config**: Simulation tests can take minutes (use slow_simulation prefix)
4. **Seed logging**: Always print the report to capture failing seeds

## Example: Adding Chaos to ActorCatalog

Here's how to add comprehensive testing to the actor catalog:

```rust
// In ActorCatalog::get_or_create()

// 1. Add buggify before directory lookup
if buggify!() {
    tracing::warn!("Buggify: Delaying directory lookup");
    self.time.sleep(Duration::from_millis(50)).await;
}

// 2. Assert on concurrent activation
if self.actors.len() > 3 {
    sometimes_assert!(
        catalog_concurrent_activations,
        true,
        "Catalog managing multiple concurrent actors"
    );
}

// 3. Add buggify in factory creation
if buggify!() {
    // Simulate slow actor initialization
    self.time.sleep(Duration::from_millis(100)).await;
}

// 4. Always assert on state consistency
always_assert!(
    catalog_no_duplicate_activation,
    !self.actors.contains_key(&actor_id),
    "Actor activated twice - race condition detected"
);
```

## Reference Files

Study these foundation files for more patterns:

- **Buggify system**: `moonpool-foundation/src/buggify.rs`
- **Assertions**: `moonpool-foundation/src/assertions.rs`
- **Ping pong actors**: `moonpool-foundation/tests/simulation/ping_pong/actors.rs`
- **Ping pong tests**: `moonpool-foundation/tests/simulation/ping_pong/tests.rs`
- **Peer networking**: `moonpool-foundation/src/network/peer/core.rs` (shows buggify in action)

## Common Patterns Summary

**Buggify Patterns**:
- Connection failures: `if buggify!() { return Err(...) }`
- Timing chaos: `let delay = if buggify!() { short } else { normal }`
- Resource limits: `let capacity = if buggify!() { 2 } else { 1000 }`

**Assertion Patterns**:
- Coverage: `sometimes_assert!(name, condition, "description")`
- Safety: `always_assert!(name, condition, "error message")`

**Simulation Patterns**:
- Multi-topology: Test 1x1, 2x2, 10x10
- Iteration control: `UntilAllSometimesReached(10_000)`
- Validation: `panic_on_assertion_violations(&report)`

Ready to add chaos testing to moonpool!
