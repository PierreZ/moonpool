# Quickstart: Location-Transparent Distributed Actor System

**Date**: 2025-10-21
**Audience**: Developers new to Moonpool actor system

## Overview

This guide demonstrates building a simple BankAccount actor that processes deposits, withdrawals, and balance queries across a distributed cluster with location transparency.

**Learning Goals**:
- Define an actor with state and methods
- Implement lifecycle hooks (activation/deactivation)
- Start an actor runtime (single-node and multi-node)
- Obtain actor references and send messages
- Handle request-response patterns with timeouts
- Error handling for actor operations

---

## Step 1: Define the Actor

Create a struct with your actor's state and methods:

```rust
use moonpool::prelude::*;
use serde::{Deserialize, Serialize};

pub struct BankAccountActor {
    actor_id: ActorId,
    balance: u64,
}

impl BankAccountActor {
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            balance: 0,
        }
    }

    /// Deposit money into account.
    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        self.balance += amount;
        Ok(self.balance)
    }

    /// Withdraw money from account.
    pub async fn withdraw(&mut self, amount: u64) -> Result<u64, ActorError> {
        if self.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: self.balance,
                requested: amount,
            });
        }
        self.balance -= amount;
        Ok(self.balance)
    }

    /// Get current balance.
    pub async fn get_balance(&self) -> Result<u64, ActorError> {
        Ok(self.balance)
    }
}
```

**Key Points**:
- Actor state stored in regular Rust fields
- Methods are normal async functions
- No locks needed (single-threaded per actor)
- Use `Result<T, ActorError>` for error handling

---

## Step 2: Implement Lifecycle Hooks

Add activation and deactivation logic with typed state:

```rust
// Define your persistent state type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BankAccountState {
    balance: u64,
}

pub struct BankAccountActor {
    actor_id: ActorId,
    state: ActorState<BankAccountState>,  // Persistent state wrapper
}

#[async_trait(?Send)]
impl Actor for BankAccountActor {
    type State = BankAccountState;  // ✅ Declare state type

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, state: Option<BankAccountState>) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} activating", self.actor_id);

        // Framework loads and deserializes state automatically
        let initial_state = state.unwrap_or(BankAccountState { balance: 0 });

        // Framework provides storage handle internally
        self.state = ActorState::new_with_storage(
            initial_state,
            self.actor_id.clone(),
            storage,  // Injected by framework
            JsonSerializer,
        );

        tracing::info!("Activated with balance: {}", self.state.get().balance);
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} deactivated: {:?} (final balance: {})",
                      self.actor_id, reason, self.state.get().balance);
        Ok(())
    }
}
```

**When Hooks Are Called**:
- `on_activate(state)`: Before processing first message, with typed state loaded by framework
- `on_deactivate(reason)`: After processing last message before removal

**State Parameter**:
- `Some(state)` - Previously persisted state (deserialized by framework)
- `None` - First activation (no state exists yet)
- Type-safe: Compiler enforces correct state type

**Deactivation Triggers**:
- Idle timeout (no messages for 10 minutes)
- Explicit deactivation request
- Node shutdown
- Activation failure

---

## Step 3: Start Actor Runtime (Single Node)

For local development and testing:

```rust
use moonpool::ActorRuntime;

#[tokio::main]
async fn main() -> Result<(), ActorError> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Create single-node actor runtime with namespace and listening address
    let runtime = ActorRuntime::builder()
        .namespace("dev")
        .listen_addr("127.0.0.1:5000")
        .build()
        .await?;
    // All actors in this runtime will have namespace "dev"

    // Use actor runtime...

    // Graceful shutdown
    runtime.shutdown(Duration::from_secs(30)).await?;

    Ok(())
}
```

**Configuration**:
- `namespace` - Defines cluster boundary (cannot send messages across namespaces)
  - Use "dev" for local development
  - Use "prod", "staging" for different environments
  - Use "tenant-{id}" for multi-tenant deployments
- `listen_addr` - Network address this node binds to for receiving messages

---

## Step 4: Obtain Actor Reference

Get a reference to an actor by type and key:

```rust
// Namespace "dev" automatically applied from runtime bootstrap
let alice: ActorRef<BankAccountActor> = runtime.get_actor("BankAccount", "alice");
// Internally creates: ActorId { namespace: "dev", actor_type: "BankAccount", key: "alice" }
// String format: "dev::BankAccount/alice"

let bob: ActorRef<BankAccountActor> = runtime.get_actor("BankAccount", "bob");
// String format: "dev::BankAccount/bob"

// Reference is valid even if actor not yet activated
// First message will trigger activation automatically
```

**ActorId Structure** (internal):
- `namespace` - Set at ActorRuntime bootstrap (e.g., "dev", "prod", "tenant-123")
- `actor_type` - Specified in `get_actor()` (e.g., "BankAccount")
- `key` - Specified in `get_actor()` (e.g., "alice", "account-456")

**String Format**: `namespace::actor_type/key`
- Examples: `dev::BankAccount/alice`, `prod::BankAccount/bob`
- `::` separates namespace from actor type
- `/` separates actor type from key
- Visible in logs, debugging output

**Key Points**:
- `get_actor()` returns immediately (synchronous)
- No network call or activation
- Type-safe: compiler enforces `BankAccountActor` methods
- Same (type, key) always returns same actor within cluster
- Namespace automatically applied (no need to specify)
- Cannot communicate across namespaces (cluster boundary)

---

## Step 5: Send Messages (Request-Response)

Call actor methods as if they were local:

```rust
// Deposit $100 into Alice's account
let balance = alice.call(DepositRequest { amount: 100 }).await?;
println!("Balance after deposit: ${}", balance);

// Withdraw $50
let balance = alice.call(WithdrawRequest { amount: 50 }).await?;
println!("Balance after withdrawal: ${}", balance);

// Check balance
let balance = alice.call(GetBalanceRequest).await?;
println!("Current balance: ${}", balance);
```

**What Happens Behind the Scenes**:
1. First call activates actor (calls `on_activate()`)
2. Message serialized to JSON
3. Directory lookup finds/assigns node
4. Message routed to correct node
5. Actor processes message sequentially
6. Response serialized and sent back
7. Response deserialized and returned

---

## Step 6: Request Messages (Typed Payloads)

Define message types for your actor methods:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetBalanceRequest;
```

**Usage**:
```rust
let response: u64 = alice.call(DepositRequest { amount: 100 }).await?;
```

---

## Step 7: Handle Timeouts

Specify custom timeout for long-running operations:

```rust
use std::time::Duration;

// Default timeout (30 seconds)
let balance = alice.call(GetBalanceRequest).await?;

// Custom timeout (5 seconds)
let balance = alice
    .call_with_timeout(GetBalanceRequest, Duration::from_secs(5))
    .await?;
```

**Timeout Errors**:
```rust
match alice.call(request).await {
    Ok(response) => println!("Got response: {:?}", response),
    Err(ActorError::Timeout) => println!("Request timed out"),
    Err(e) => println!("Other error: {:?}", e),
}
```

---

## Step 8: One-Way Messages (Fire-and-Forget)

Send messages without waiting for response:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct DepositNotification {
    pub amount: u64,
}

// Send notification (does not block)
alice.send(DepositNotification { amount: 100 }).await?;

// Continues immediately without waiting for processing
println!("Notification sent!");
```

**When to Use**:
- Notifications
- Commands where response not needed
- Best-effort delivery

---

## Step 9: Error Handling

Handle common actor errors:

```rust
match alice.call(WithdrawRequest { amount: 1000 }).await {
    Ok(balance) => {
        println!("Withdrawal successful, new balance: ${}", balance);
    }
    Err(ActorError::InsufficientFunds { available, requested }) => {
        println!("Insufficient funds: have ${}, need ${}", available, requested);
    }
    Err(ActorError::Timeout) => {
        println!("Request timed out after 30 seconds");
    }
    Err(ActorError::ActivationFailed(reason)) => {
        println!("Actor failed to activate: {}", reason);
    }
    Err(ActorError::NodeUnavailable(node_id)) => {
        println!("Node {:?} is unavailable", node_id);
    }
    Err(e) => {
        println!("Unexpected error: {:?}", e);
    }
}
```

**Common Errors**:
- `InsufficientFunds` - Application-specific error
- `Timeout` - Response not received within timeout
- `ActivationFailed` - Actor's `on_activate()` returned error
- `NodeUnavailable` - Target node unreachable
- `ProcessingFailed` - Actor method threw exception

---

## Step 10: Multi-Node Cluster

Start a 3-node cluster sharing the same directory and storage:

```rust
use moonpool::storage::InMemoryStorage;
use moonpool::directory::SimpleDirectory;

// Create shared infrastructure
let directory = Arc::new(SimpleDirectory::new());
let storage = Arc::new(InMemoryStorage::new());

// Node 1 at 127.0.0.1:5000
let node1 = ActorRuntime::builder()
    .namespace("prod")
    .listen_addr("127.0.0.1:5000")
    .directory(directory.clone())
    .storage(storage.clone())
    .build()
    .await?;

// Node 2 at 127.0.0.1:5001 (separate process or task)
let node2 = ActorRuntime::builder()
    .namespace("prod")
    .listen_addr("127.0.0.1:5001")
    .directory(directory.clone())  // Same directory
    .storage(storage.clone())       // Same storage
    .build()
    .await?;

// Node 3 at 127.0.0.1:5002 (separate process or task)
let node3 = ActorRuntime::builder()
    .namespace("prod")
    .listen_addr("127.0.0.1:5002")
    .directory(directory.clone())  // Same directory
    .storage(storage.clone())       // Same storage
    .build()
    .await?;
```

**Why Share Directory and Storage?**:
- **Directory**: All nodes need to know where actors are located
- **Storage**: All nodes need access to the same actor state
- **Namespace**: Defines cluster boundary (all nodes use "prod")

**Location Transparency**:
```rust
// On any node, call any actor (namespace "prod" automatic)
let alice = node1.get_actor("BankAccount", "alice");
// Creates: "prod::BankAccount/alice"

let bob = node2.get_actor("BankAccount", "bob");
// Creates: "prod::BankAccount/bob"

// Actors automatically placed across nodes
// Messages automatically routed
// No code changes needed!
```

**Multi-Tenant Deployment**:
```rust
// Tenant ACME gets own cluster with "tenant-acme" namespace
let acme_runtime = ActorRuntime::builder()
    .namespace("tenant-acme")
    .listen_addr("127.0.0.1:6000")
    .build()
    .await?;
let alice_acme = acme_runtime.get_actor("BankAccount", "alice");
// Creates: "tenant-acme::BankAccount/alice"

// Tenant Globex gets own cluster with "tenant-globex" namespace
let globex_runtime = ActorRuntime::builder()
    .namespace("tenant-globex")
    .listen_addr("127.0.0.1:7000")
    .build()
    .await?;
let alice_globex = globex_runtime.get_actor("BankAccount", "alice");
// Creates: "tenant-globex::BankAccount/alice"

// These are DIFFERENT actors - namespace provides isolation
```

---

## Step 11: Persistence (Automatic with ActorState)

State persistence is handled automatically through `ActorState<T>` wrapper:

### Configure Runtime with Storage

```rust
use moonpool::storage::InMemoryStorage;

// Create storage provider
let storage = Arc::new(InMemoryStorage::new());

// Start runtime with storage
let runtime = ActorRuntime::builder()
    .namespace("dev")
    .listen_addr("127.0.0.1:5000")
    .storage(storage)
    .build()
    .await?;
```

### Actor Methods with Persistence

```rust
impl BankAccountActor {
    /// Deposit money and persist new balance
    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        // Clone current state
        let mut data = self.state.get().clone();

        // Mutate
        data.balance += amount;

        // Persist (serialize + save + update in-memory automatically)
        self.state.persist(data).await?;

        Ok(self.state.get().balance)
    }

    /// Withdraw money and persist new balance
    pub async fn withdraw(&mut self, amount: u64) -> Result<u64, ActorError> {
        let mut data = self.state.get().clone();

        if data.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: data.balance,
                requested: amount,
            });
        }

        // Mutate
        data.balance -= amount;

        // Persist
        self.state.persist(data).await?;

        Ok(self.state.get().balance)
    }

    /// Get balance (read from memory - no storage access)
    pub async fn get_balance(&self) -> Result<u64, ActorError> {
        Ok(self.state.get().balance)
    }
}
```

### Key Differences from Manual Storage

- ✅ **No manual serialization** - Framework handles serde
- ✅ **Type-safe** - Compiler checks state type
- ✅ **Clean API** - `persist(data)` instead of manual `save_state()`
- ✅ **Automatic deserialization** - State loaded in `on_activate()`
- ✅ **Atomic updates** - In-memory state updated only after successful save

### Handle Storage Errors

```rust
match alice.call(DepositRequest { amount: 100 }).await {
    Ok(balance) => {
        println!("Deposit successful, new balance: ${}", balance);
    }
    Err(ActorError::StorageFailed(StorageError::Unavailable)) => {
        println!("Storage temporarily unavailable - retry later");
    }
    Err(ActorError::StorageFailed(e)) => {
        println!("Storage error: {}", e);
    }
    Err(e) => {
        println!("Other error: {:?}", e);
    }
}
```

### Test State Persistence

```rust
// Deposit money
alice.call(DepositRequest { amount: 500 }).await?;

// Deactivate actor
runtime.deactivate_actor(&alice.actor_id()).await?;

// Reactivate by sending new message (framework loads persisted state)
let balance = alice.call(GetBalanceRequest).await?;
assert_eq!(balance, 500); // State persisted!
```

---

## Complete Example (Without Persistence)

```rust
use moonpool::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// 1. Define actor
pub struct BankAccountActor {
    actor_id: ActorId,
    balance: u64,
}

impl BankAccountActor {
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            balance: 0,
        }
    }

    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        self.balance += amount;
        Ok(self.balance)
    }

    pub async fn withdraw(&mut self, amount: u64) -> Result<u64, ActorError> {
        if self.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: self.balance,
                requested: amount,
            });
        }
        self.balance -= amount;
        Ok(self.balance)
    }

    pub async fn get_balance(&self) -> Result<u64, ActorError> {
        Ok(self.balance)
    }
}

// 2. Implement lifecycle hooks
#[async_trait(?Send)]
impl Actor for BankAccountActor {
    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(
        &mut self,
        _storage: Option<&dyn StorageProvider>,
    ) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} activated", self.actor_id.to_string());
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} deactivated: {:?}", self.actor_id.to_string(), reason);
        Ok(())
    }
}

// 3. Define message types
#[derive(Debug, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetBalanceRequest;

// 4. Main function
#[tokio::main]
async fn main() -> Result<(), ActorError> {
    tracing_subscriber::fmt::init();

    // Start actor runtime with "dev" namespace
    let runtime = ActorRuntime::builder()
        .namespace("dev")
        .listen_addr("127.0.0.1:5000")
        .build()
        .await?;

    // Get actor references (namespace "dev" automatically applied)
    let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
    let bob = runtime.get_actor::<BankAccountActor>("BankAccount", "bob");

    // Perform operations
    let balance = alice.call(DepositRequest { amount: 100 }).await?;
    println!("Alice deposited $100, balance: ${}", balance);

    let balance = bob.call(DepositRequest { amount: 200 }).await?;
    println!("Bob deposited $200, balance: ${}", balance);

    let balance = alice.call(WithdrawRequest { amount: 30 }).await?;
    println!("Alice withdrew $30, balance: ${}", balance);

    let balance = alice.call(GetBalanceRequest).await?;
    println!("Alice final balance: ${}", balance);

    let balance = bob.call(GetBalanceRequest).await?;
    println!("Bob final balance: ${}", balance);

    // Graceful shutdown
    runtime.shutdown(Duration::from_secs(30)).await?;

    Ok(())
}
```

---

## Complete Example (With Persistence)

```rust
use moonpool::prelude::*;
use moonpool::storage::InMemoryStorage;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// 1. Define persistent state type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BankAccountState {
    balance: u64,
}

// 2. Define actor with ActorState wrapper
pub struct BankAccountActor {
    actor_id: ActorId,
    state: ActorState<BankAccountState>,
}

impl BankAccountActor {
    /// Deposit with automatic persistence
    pub async fn deposit(&mut self, amount: u64) -> Result<u64, ActorError> {
        let mut data = self.state.get().clone();
        data.balance += amount;
        self.state.persist(data).await?;
        Ok(self.state.get().balance)
    }

    /// Withdraw with automatic persistence
    pub async fn withdraw(&mut self, amount: u64) -> Result<u64, ActorError> {
        let mut data = self.state.get().clone();

        if data.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: data.balance,
                requested: amount,
            });
        }

        data.balance -= amount;
        self.state.persist(data).await?;
        Ok(self.state.get().balance)
    }

    pub async fn get_balance(&self) -> Result<u64, ActorError> {
        Ok(self.state.get().balance)
    }
}

// 3. Implement lifecycle hooks with typed state
#[async_trait(?Send)]
impl Actor for BankAccountActor {
    type State = BankAccountState;

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, state: Option<BankAccountState>) -> Result<(), ActorError> {
        let initial_state = state.unwrap_or(BankAccountState { balance: 0 });

        self.state = ActorState::new_with_storage(
            initial_state,
            self.actor_id.clone(),
            storage,  // Injected by framework
            JsonSerializer,
        );

        tracing::info!("Activated with balance: {}", self.state.get().balance);
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("Deactivated: {:?}, final balance: {}",
                      reason, self.state.get().balance);
        Ok(())
    }
}

// 4. Define message types
#[derive(Debug, Serialize, Deserialize)]
pub struct DepositRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WithdrawRequest {
    pub amount: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetBalanceRequest;

// 5. Main function with storage
#[tokio::main]
async fn main() -> Result<(), ActorError> {
    tracing_subscriber::fmt::init();

    // Create storage provider
    let storage = Arc::new(InMemoryStorage::new());

    // Start actor runtime with storage
    let runtime = ActorRuntime::builder()
        .namespace("dev")
        .listen_addr("127.0.0.1:5000")
        .storage(storage)
        .build()
        .await?;

    // Get actor references
    let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");

    // Perform operations (state persisted automatically)
    let balance = alice.call(DepositRequest { amount: 100 }).await?;
    println!("After deposit: ${}", balance);

    let balance = alice.call(DepositRequest { amount: 200 }).await?;
    println!("After second deposit: ${}", balance);

    // Force deactivation to test persistence
    runtime.deactivate_actor(&alice.actor_id()).await?;
    println!("Actor deactivated");

    // Reactivate by sending new message (framework loads persisted state)
    let balance = alice.call(GetBalanceRequest).await?;
    println!("After reactivation: ${}", balance); // Should be 300!

    // Graceful shutdown
    runtime.shutdown(Duration::from_secs(30)).await?;

    Ok(())
}
```

**Expected Output**:
```
INFO Activated with balance: 0
After deposit: $100
After second deposit: $300
INFO Deactivated: ExplicitRequest, final balance: 300
Actor deactivated
INFO Activated with balance: 300
After reactivation: $300
INFO Deactivated: NodeShutdown, final balance: 300
```

**Key Differences from Non-Persistent Version**:
1. Separate `BankAccountState` type with `#[derive(Serialize, Deserialize)]`
2. Actor stores `ActorState<BankAccountState>` wrapper instead of raw fields
3. `type State = BankAccountState` declares state type in Actor trait
4. Framework loads/deserializes state, passes to `on_activate(state)`
5. `persist()` called explicitly in methods (serialize + save + update)
6. No manual serde calls - framework handles all serialization

---

## Key Concepts

### Location Transparency
Actors accessed by ID, not node location. System handles routing automatically.

### Automatic Activation
First message triggers actor creation. No manual lifecycle management.

### Single-Threaded Execution
Messages processed one-at-a-time per actor. No locks needed in actor code.

### Request-Response Pattern
`call()` waits for response with timeout. `send()` is fire-and-forget.

### Lifecycle Hooks
`on_activate()` and `on_deactivate()` for resource management.

---

## Next Steps

- **Testing**: Write simulation tests with buggify for chaos testing
- **Persistence**: Add state persistence in lifecycle hooks (future feature)
- **Complex Actors**: Build multi-actor workflows (transfers, notifications)
- **Error Recovery**: Handle network failures and timeouts gracefully
- **Performance**: Tune placement algorithm and timeout configurations

---

## Common Patterns

### Transfer Between Accounts
```rust
async fn transfer(
    from: ActorRef<BankAccountActor>,
    to: ActorRef<BankAccountActor>,
    amount: u64,
) -> Result<(), ActorError> {
    // Withdraw from source
    from.call(WithdrawRequest { amount }).await?;

    // Deposit to destination
    to.call(DepositRequest { amount }).await?;

    Ok(())
}

// Usage
transfer(alice, bob, 50).await?;
```

**Note**: This is not atomic. For transactional transfers, see future persistence/transactions feature.

### Actor-to-Actor Communication

When an actor needs to call another actor, it can accept an `ActorContext` parameter to obtain actor references:

```rust
impl BankAccountActor {
    /// Transfer money to another account (actor obtains recipient reference)
    pub async fn transfer_to(
        &mut self,
        ctx: &ActorContext,
        recipient_key: &str,
        amount: u64,
    ) -> Result<(), ActorError> {
        // Withdraw from self
        if self.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: self.balance,
                requested: amount,
            });
        }
        self.balance -= amount;

        // Get reference to recipient actor via context
        let recipient: ActorRef<BankAccountActor> = ctx.get_actor("BankAccount", recipient_key);

        // Deposit to recipient
        recipient.call(DepositRequest { amount }).await?;

        Ok(())
    }
}

// Usage
let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice");
alice.call(TransferRequest {
    recipient_key: "bob".to_string(),
    amount: 50,
}).await?;
```

**Alternative: Pass ActorRef directly** (when caller already has reference):
```rust
impl BankAccountActor {
    pub async fn transfer_to_ref(
        &mut self,
        recipient: ActorRef<BankAccountActor>,
        amount: u64,
    ) -> Result<(), ActorError> {
        if self.balance < amount {
            return Err(ActorError::InsufficientFunds {
                available: self.balance,
                requested: amount,
            });
        }
        self.balance -= amount;
        recipient.call(DepositRequest { amount }).await?;
        Ok(())
    }
}
```

**ActorContext API**:
- `get_actor<A>(actor_type, key)` - Get reference to any actor in the cluster
- Automatically uses the runtime's namespace
- Lightweight (no storage overhead in actor struct)
- Testable (can pass mock context in tests)

### Timeout Strategies
```rust
// Retry with exponential backoff
let mut delay = Duration::from_millis(100);
for attempt in 0..3 {
    match alice.call_with_timeout(request.clone(), Duration::from_secs(5)).await {
        Ok(response) => return Ok(response),
        Err(ActorError::Timeout) if attempt < 2 => {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        Err(e) => return Err(e),
    }
}
```

---

## Troubleshooting

### Actor Never Activates
**Symptom**: Timeout on first message
**Causes**:
- `on_activate()` threw exception
- Node not reachable
- Directory unavailable

**Solution**: Check logs for activation errors, verify network connectivity

### Messages Timing Out
**Symptom**: `ActorError::Timeout` on calls
**Causes**:
- Actor processing too slow
- Network latency
- Timeout too short

**Solution**: Increase timeout, check actor method performance

### Balance Invariant Violated
**Symptom**: Sum of balances doesn't match expected
**Causes**:
- Transfer not atomic (withdraw succeeded, deposit failed)
- Concurrent operations without coordination

**Solution**: Use transactions feature (future) or serialize operations

---

## References

- **API Contracts**: `specs/001-location-transparent-actors/contracts/*.rs`
- **Data Model**: `specs/001-location-transparent-actors/data-model.md`
- **Implementation Plan**: `specs/001-location-transparent-actors/plan.md`
- **Research**: `specs/001-location-transparent-actors/research.md`
