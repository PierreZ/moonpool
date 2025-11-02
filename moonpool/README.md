# Moonpool: Location-Transparent Distributed Actor System

⚠️ **EARLY ALPHA - EXPERIMENTAL**: This crate is in early development. Core features are functional but the project lacks production hardening, comprehensive integration tests, and monitoring. Use at your own risk for hobby projects only.

## Overview

Moonpool is a virtual actor system inspired by Microsoft Orleans, providing location-transparent actors with automatic activation and state persistence. Built on the [moonpool-foundation](../moonpool-foundation) deterministic simulation framework for reliable testing.

## Features

### Core Actor System

- **Location Transparency**: Actors accessed by ID, not physical location
  - `ActorRef<T>` provides typed references to actors
  - Automatic routing to correct node via Directory
  - Transparent remote/local dispatch

- **Automatic Activation**: Actors created on-demand
  - Factory pattern for actor instantiation
  - Double-check locking in ActorCatalog prevents races
  - State automatically loaded from storage on activation

- **Message Processing**: Single-threaded per-actor execution
  - `ActorContext` manages message queue and lifecycle
  - Sequential message processing guarantees
  - Error isolation per actor

- **State Persistence**: Pluggable storage system
  - `ActorState<T>` wrapper for automatic serialization
  - Storage trait for custom backends
  - JSON serialization by default

### Distribution

- **Multi-Node Runtime**: Cluster support out of the box
  - ActorRuntime builder with namespace and listen_addr
  - Shared Directory for cluster-wide location tracking
  - Network transport via moonpool-foundation

- **Placement Strategies**: Configurable actor placement
  - Random, Local, LeastLoaded algorithms
  - PlacementHint for application control
  - Extensible via Placement trait

- **MessageBus**: Reliable message routing
  - Request-response with correlation IDs
  - One-way messages for fire-and-forget
  - Automatic local vs remote dispatch
  - Timeout handling and error propagation

## Quick Start

### Define an Actor

```rust
use moonpool::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BankAccountState {
    balance: u64,
}

struct BankAccountActor {
    actor_id: ActorId,
    state: ActorState<BankAccountState>,
}

#[async_trait(?Send)]
impl Actor for BankAccountActor {
    type State = BankAccountState;

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, state: Option<BankAccountState>) -> Result<()> {
        // State automatically loaded from storage
        let initial = state.unwrap_or(BankAccountState { balance: 0 });
        self.state = ActorState::new(initial);
        Ok(())
    }

    async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<()> {
        // State automatically persisted on deactivation
        Ok(())
    }
}

// Implement message handlers
impl BankAccountActor {
    async fn deposit(&mut self, amount: u64) -> Result<u64> {
        self.state.balance += amount;
        self.state.mark_modified(); // Flag for persistence
        Ok(self.state.balance)
    }

    async fn get_balance(&self) -> Result<u64> {
        Ok(self.state.balance)
    }
}
```

### Create Extension Trait for Type-Safe Calls

```rust
use moonpool::actor::reference::ActorRef;

#[async_trait(?Send)]
trait BankAccountActorRef {
    async fn deposit(&self, amount: u64) -> Result<u64>;
    async fn get_balance(&self) -> Result<u64>;
}

#[async_trait(?Send)]
impl BankAccountActorRef for ActorRef<BankAccountActor> {
    async fn deposit(&self, amount: u64) -> Result<u64> {
        self.call("deposit", amount).await
    }

    async fn get_balance(&self) -> Result<u64> {
        self.call("get_balance", ()).await
    }
}
```

### Start Runtime and Use Actors

```rust
use moonpool::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Create local task set for !Send actors
    let local = tokio::task::LocalSet::new();

    local.run_until(async {
        // Build actor runtime
        let runtime = ActorRuntime::builder()
            .namespace("example")
            .listen_addr("127.0.0.1:5000")?
            .with_providers(
                TokioNetworkProvider,
                TokioTimeProvider,
                TokioTaskProvider,
            )
            .build()
            .await?;

        // Register actor type with factory
        runtime.register_actor::<BankAccountActor, _>(BankAccountActorFactory)?;

        // Get actor reference (location-transparent)
        let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;

        // Call actor methods (auto-activation, state persistence)
        let balance = alice.deposit(100).await?;
        println!("Balance: ${}", balance);

        Ok::<_, ActorError>(())
    }).await?;

    Ok(())
}
```

## Examples

The `examples/` directory contains complete working demonstrations:

### hello_actor

Multi-node virtual actor demonstration:

```bash
# Single node
cargo run --example hello_actor

# Multi-node cluster (3 nodes, 10 random greetings)
cargo run --example hello_actor -- --nodes 3 --count 10 --seed 42
```

**Features demonstrated:**
- Auto-activation of actors on first message
- Directory-based placement across nodes
- Extension trait pattern for type-safe calls
- Deterministic random name generation

### bank_account

State persistence with DeactivateOnIdle policy:

```bash
# Single node
cargo run --example bank_account

# Multi-node cluster with 15 operations
cargo run --example bank_account -- --nodes 3 --operations 15 --seed 123
```

**Features demonstrated:**
- Automatic state loading on activation
- State persistence after each operation
- Deactivation when actor queue is empty
- State reloading on reactivation (possibly on different node!)
- Multi-node deployment with shared storage

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                  ActorRuntime                           │
│  (namespace, listen_addr, directory, storage)           │
└─────────────┬──────────────────────────┬────────────────┘
              │                          │
              ├──────────────┐           ├──────────────┐
              ▼              ▼           ▼              ▼
       ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌───────────┐
       │ ActorCatalog│  │ MessageBus│  │ Directory │  │  Storage  │
       │ (local     │  │ (routing  │  │ (location)│  │ (persist) │
       │  registry) │  │  +        │  │           │  │           │
       │            │  │correlation)│  │           │  │           │
       └─────┬──────┘  └───────────┘  └───────────┘  └───────────┘
             │
             ▼
       ┌───────────────────┐
       │  ActorContext<A>  │
       │  (state, queue,   │
       │   lifecycle)      │
       └───────────────────┘
```

## Implementation Status

### ✅ Implemented

- **ActorCatalog**: Local actor registry with double-check locking (1,278 lines, tested)
- **MessageBus**: Full routing with correlation IDs (1,156 lines, tested)
- **ActorContext**: Per-actor message loop and lifecycle (821 lines, tested)
- **ActorRuntime**: Multi-node runtime with builder pattern (481 lines, tested)
- **Directory**: Location tracking with placement strategies (420 lines, tested)
- **Storage**: Pluggable persistence with in-memory implementation (350 lines, tested)
- **Serialization**: JSON by default, extensible via trait (311 lines, tested)
- **Network Integration**: Built on moonpool-foundation transport
- **Tests**: 76 unit tests passing
- **Examples**: 2 working multi-node examples

### ⚠️ Needs Work

- **Integration Tests**: Limited simulation-based testing
- **Monitoring**: No metrics or telemetry yet
- **Idle Timeout**: Policy exists but not fully implemented
- **Graceful Shutdown**: Stub implementation
- **Replication**: Not implemented
- **Production Hardening**: Error handling needs review

## Testing

Run the test suite:

```bash
# All tests
nix develop --command cargo nextest run -p moonpool

# Specific test
nix develop --command cargo nextest run -p moonpool test_message_bus_routing
```

Run examples to validate multi-node scenarios:

```bash
nix develop --command cargo run --example hello_actor -- --nodes 2
nix develop --command cargo run --example bank_account -- --nodes 2
```

## Documentation

- **API Docs**: Run `cargo doc --open -p moonpool`
- **Main Specification**: See `../docs/specs/` for technical details
- **Orleans Analysis**: See `../docs/analysis/orleans/` for reference architecture
- **Development Guide**: See `CLAUDE.md` for contributor instructions

## Related Crates

- [moonpool-foundation](../moonpool-foundation): The deterministic simulation framework this actor system is built on

## License

Apache 2.0
