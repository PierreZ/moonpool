# Quickstart: Location-Transparent Distributed Actor System

**Date**: 2025-10-21
**Audience**: Developers new to Moonpool actor system

## Overview

This guide demonstrates building a simple BankAccount actor that processes deposits, withdrawals, and balance queries across a distributed cluster with location transparency.

**Learning Goals**:
- Define an actor with state and methods
- Implement lifecycle hooks (activation/deactivation)
- Start an actor system (single-node and multi-node)
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

Add activation and deactivation logic:

```rust
#[async_trait(?Send)]
impl Actor for BankAccountActor {
    async fn on_activate(&mut self) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} activated with balance {}",
                      self.actor_id, self.balance);
        // Initialize resources here (e.g., load state from storage)
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        tracing::info!("BankAccount {} deactivated: {:?} (balance: {})",
                      self.actor_id, reason, self.balance);
        // Clean up resources here (e.g., persist state)
        Ok(())
    }
}
```

**When Hooks Are Called**:
- `on_activate()`: Before processing first message
- `on_deactivate()`: After processing last message before removal

**Deactivation Triggers**:
- Idle timeout (no messages for 10 minutes)
- Explicit deactivation request
- Node shutdown
- Activation failure

---

## Step 3: Start Actor System (Single Node)

For local development and testing:

```rust
use moonpool::ActorSystem;

#[tokio::main]
async fn main() -> Result<(), ActorError> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Create single-node actor system with namespace
    let system = ActorSystem::single_node("dev").await?;
    // All actors in this system will have namespace "dev"

    // Use actor system...

    // Graceful shutdown
    system.shutdown(Duration::from_secs(30)).await?;

    Ok(())
}
```

**Namespace**:
- Defines cluster boundary (cannot send messages across namespaces)
- Use "dev" for local development
- Use "prod", "staging" for different environments
- Use "tenant-{id}" for multi-tenant deployments

---

## Step 4: Obtain Actor Reference

Get a reference to an actor by type and key:

```rust
// Namespace "dev" automatically applied from system bootstrap
let alice: ActorRef<BankAccountActor> = system.get_actor("BankAccount", "alice");
// Internally creates: ActorId { namespace: "dev", actor_type: "BankAccount", key: "alice" }
// String format: "dev::BankAccount/alice"

let bob: ActorRef<BankAccountActor> = system.get_actor("BankAccount", "bob");
// String format: "dev::BankAccount/bob"

// Reference is valid even if actor not yet activated
// First message will trigger activation automatically
```

**ActorId Structure** (internal):
- `namespace` - Set at ActorSystem bootstrap (e.g., "dev", "prod", "tenant-123")
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

Start a 3-node cluster (all nodes must use same namespace):

```rust
// Node 0
let system = ActorSystem::multi_node(
    "prod",  // Namespace - same across all nodes in cluster
    NodeId(0),
    vec![
        "127.0.0.1:5000",  // Node 0 (this node)
        "127.0.0.1:5001",  // Node 1
        "127.0.0.1:5002",  // Node 2
    ],
).await?;

// Node 1 (separate process) - SAME namespace "prod"
let system = ActorSystem::multi_node(
    "prod",  // Must match!
    NodeId(1),
    vec![
        "127.0.0.1:5000",
        "127.0.0.1:5001",  // Node 1 (this node)
        "127.0.0.1:5002",
    ],
).await?;

// Node 2 (separate process) - SAME namespace "prod"
let system = ActorSystem::multi_node(
    "prod",  // Must match!
    NodeId(2),
    vec![
        "127.0.0.1:5000",
        "127.0.0.1:5001",
        "127.0.0.1:5002",  // Node 2 (this node)
    ],
).await?;
```

**Location Transparency**:
```rust
// On any node, call any actor (namespace "prod" automatic)
let alice = system.get_actor("BankAccount", "alice");
// Creates: "prod::BankAccount/alice"

let bob = system.get_actor("BankAccount", "bob");
// Creates: "prod::BankAccount/bob"

// Actors automatically placed across nodes
// Messages automatically routed
// No code changes needed!
```

**Multi-Tenant Deployment**:
```rust
// Tenant ACME gets own cluster with "tenant-acme" namespace
let acme_system = ActorSystem::single_node("tenant-acme").await?;
let alice_acme = acme_system.get_actor("BankAccount", "alice");
// Creates: "tenant-acme::BankAccount/alice"

// Tenant Globex gets own cluster with "tenant-globex" namespace
let globex_system = ActorSystem::single_node("tenant-globex").await?;
let alice_globex = globex_system.get_actor("BankAccount", "alice");
// Creates: "tenant-globex::BankAccount/alice"

// These are DIFFERENT actors - namespace provides isolation
```

---

## Complete Example

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
    async fn on_activate(&mut self) -> Result<(), ActorError> {
        // String format: "default::BankAccount/alice"
        tracing::info!("BankAccount {} activated", self.actor_id.to_string());
        Ok(())
    }

    async fn on_deactivate(&mut self, reason: DeactivationReason) -> Result<(), ActorError> {
        // String format: "default::BankAccount/alice"
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

    // Start actor system with "dev" namespace
    let system = ActorSystem::single_node("dev").await?;

    // Get actor references (namespace "dev" automatically applied)
    let alice = system.get_actor::<BankAccountActor>("BankAccount", "alice");
    let bob = system.get_actor::<BankAccountActor>("BankAccount", "bob");

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
    system.shutdown(Duration::from_secs(30)).await?;

    Ok(())
}
```

**Expected Output**:
```
INFO BankAccount dev::BankAccount/alice activated
INFO BankAccount dev::BankAccount/bob activated
Alice deposited $100, balance: $100
Bob deposited $200, balance: $200
Alice withdrew $30, balance: $70
Alice final balance: $70
Bob final balance: $200
INFO BankAccount dev::BankAccount/alice deactivated: NodeShutdown
INFO BankAccount dev::BankAccount/bob deactivated: NodeShutdown
```

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
```rust
impl BankAccountActor {
    pub async fn transfer_to(
        &mut self,
        recipient: ActorRef<BankAccountActor>,
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

        // Deposit to recipient
        recipient.call(DepositRequest { amount }).await?;

        Ok(())
    }
}
```

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
